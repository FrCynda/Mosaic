//! View > Icons…: what a tile shows for a folder holding one kind of file (that kind's Windows folder, what's
//! inside, or both), and which arrow marks a shortcut, how big.
//! Saved and shown at once.

use std::cell::RefCell;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, ScreenToClient, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gfx::{Bitmap, Canvas, Gfx};
use super::themes::ink;
use super::{png_pixels, put, put_mid, t, theme, w, wz, xy, Px, ARROWS, ARROW_SIZES};

const WM_PREVIEWS: u32 = WM_APP + 1; // lParam: a boxed [Option<Px>; 3], the folder as Type, Contents, Both
const SAMPLE: &[u8] = include_bytes!("../icons/sample-photo.png");

// the layout, in CSS px from the top: the folders' section, a heading, three choices (each showing a folder of
// pictures that way) and what the chosen one does,
// then the arrows (a size slider beside their heading), each a swatch with its name, five to a row
const WIDTH: i32 = 520;
const PAD: i32 = 24;
const SEG_H: i32 = 104;
const PREVIEW: i32 = 64;
const SECTION: [i32; 2] = [70, 266];
const SWATCH: i32 = 70;
const PER_ROW: usize = 5;
const ARROW: i32 = 32;
const FOOT_TOP: i32 = SECTION[1] + 26 + 2 * SWATCH + 8 + 34;
const HEIGHT: i32 = FOOT_TOP + 34 + PAD;

// per section: heading, then (label, what it does) for each value
const TEXT: [(&str, [(&str, &str); 3]); 1] = [
    ("icons.folders", [("icons.type", "icons.type_hint"), ("icons.contents", "icons.contents_hint"), ("icons.both", "icons.both_hint")]),
];
// (key, or a name said the same in every language)
const ARROW_NAMES: [(&str, &str); 10] = [
    ("", "Windows 11"),
    ("icons.arrow_dark", ""),
    ("icons.arrow_classic", ""),
    ("icons.arrow_plain", ""),
    ("icons.arrow_badge", ""),
    ("icons.arrow_box", ""),
    ("icons.arrow_chain", ""),
    ("icons.arrow_corner", ""),
    ("", "macOS"),
    ("", "Windows 95"),
];

#[derive(Clone, Copy, PartialEq)]
enum Hot {
    Seg(usize, u8),
    Arrow(u8),
    Size,
    Done,
}

struct Dlg {
    hwnd: HWND,
    owner: HWND,
    gfx: Gfx,
    cv: Canvas,
    scale: f64,
    vals: [u8; 2], // folder_icons, link_arrow
    arrows: (u32, Vec<Option<Bitmap>>), // the swatches' arrows, made on this canvas (its `gen`)
    previews: Option<[Option<Px>; 3]>, // a folder of pictures as each choice shows it
    shown: (u32, Vec<Option<Bitmap>>),  // … made on this canvas
    size: u8,      // link_size
    sliding: bool, // the size knob held (the mouse captured)
    hot: Option<Hot>,
    press: Option<Hot>,
    tracking: bool,
}

thread_local!(static DLG: RefCell<Option<Dlg>> = const { RefCell::new(None) });

fn with<R>(f: impl FnOnce(&mut Dlg) -> R) -> Option<R> {
    DLG.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

impl Dlg {
    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.scale).round() as i32
    }

    /// Choice `i` of section `k` (the three share their edges).
    fn seg(&self, k: usize, i: u8) -> RECT {
        let (l, r) = (self.px(PAD), self.px(WIDTH - PAD));
        let edge = |i: i32| l + (r - l) * i / 3;
        let i = i as i32;
        let top = self.px(SECTION[k] + 26);
        RECT { left: edge(i) - (i > 0) as i32, top, right: edge(i + 1), bottom: top + self.px(SEG_H) }
    }

    /// Arrow `i`'s swatch.
    fn swatch(&self, i: u8) -> RECT {
        let (l, r) = (self.px(PAD), self.px(WIDTH - PAD));
        let (col, row) = ((i as usize % PER_ROW) as i32, (i as usize / PER_ROW) as i32);
        let edge = |i: i32| l + (r - l + self.px(8)) * i / PER_ROW as i32;
        let top = self.px(SECTION[1] + 26 + row * (SWATCH + 8));
        RECT { left: edge(col), top, right: edge(col + 1) - self.px(8), bottom: top + self.px(SWATCH) }
    }

    /// The size slider's track (as tall as its row, for the mouse), right of the arrows' heading after "Size".
    fn track(&self) -> RECT {
        let g = &self.gfx;
        let (mid, r) = (self.px(WIDTH / 2), self.px(WIDTH - PAD));
        let cy = self.px(SECTION[1]) + g.f.lh / 2;
        RECT { left: mid + g.width(t("icons.arrow_size"), &g.f.reg) + self.px(16), top: cy - self.px(14), right: r - self.px(52), bottom: cy + self.px(14) }
    }

    /// The knob to x: the size it stands for, in steps of 10 %, shown at once (saved when let go).
    fn slide(&mut self, x: i32) {
        let tr = self.track();
        let (a, b) = (*ARROW_SIZES.start() as f64, *ARROW_SIZES.end() as f64);
        let f = ((x - tr.left) as f64 / (tr.right - tr.left).max(1) as f64).clamp(0.0, 1.0);
        self.resize(((a + f * (b - a)) / 10.0).round() as u8 * 10, false);
    }

    fn resize(&mut self, v: u8, save: bool) {
        let v = v.clamp(*ARROW_SIZES.start(), *ARROW_SIZES.end());
        if v != self.size || save {
            self.size = v;
            super::with(|u| {
                u.link_size = v;
                u.arrow_changed(save);
            });
            unsafe { InvalidateRect(self.hwnd, null(), 0) };
        }
    }

    fn done(&self) -> RECT {
        let g = &self.gfx;
        let r = self.px(WIDTH - PAD);
        let w = (g.width(t("apps.done"), &g.f.semi) + self.px(32)).max(self.px(104));
        RECT { left: r - w, top: self.px(FOOT_TOP), right: r, bottom: self.px(FOOT_TOP + 34) }
    }

    fn hit(&self, x: i32, y: i32) -> Option<Hot> {
        let inside = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        if inside(&self.done()) {
            return Some(Hot::Done);
        }
        if inside(&self.track()) {
            return Some(Hot::Size);
        }
        if let Some(i) = (0..ARROWS.len() as u8).find(|&i| inside(&self.swatch(i))) {
            return Some(Hot::Arrow(i));
        }
        (0..TEXT.len()).flat_map(|k| (0..3).map(move |i| (k, i))).find(|&(k, i)| inside(&self.seg(k, i))).map(|(k, i)| Hot::Seg(k, i))
    }

    fn act(&mut self, k: usize, v: u8) {
        if self.vals[k] == v {
            return;
        }
        self.vals[k] = v;
        super::with(|u| {
            match k {
                0 => u.folder_icons = v,
                _ => {
                    u.link_arrow = v;
                    return u.arrow_changed(true); // only the arrows made again, not every icon
                }
            }
            u.icons_changed();
        });
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn paint(&mut self) {
        if self.arrows.0 != self.cv.gen || self.arrows.1.is_empty() {
            let s = self.px(ARROW);
            self.arrows = (self.cv.gen, ARROWS.iter().map(|b| png_pixels(b, s).and_then(|(px, w, h)| self.cv.make_bitmap(&px, w, h))).collect());
        }
        if let Some(p) = self.previews.as_ref().filter(|_| self.shown.0 != self.cv.gen || self.shown.1.is_empty()) {
            self.shown = (self.cv.gen, p.iter().map(|x| x.as_ref().and_then(|(px, w, h)| self.cv.make_bitmap(px, *w, *h))).collect());
        }
        if !self.cv.begin() {
            return;
        }
        let (g, cv, s, lh) = (&self.gfx, &self.cv, self.scale, self.gfx.f.lh);
        let px = |v: i32| (v as f64 * s).round() as i32;
        let rad = |v: f64| (v * s) as f32;
        let th = theme();
        let mut c: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut c) };
        let hot = |h: Hot| self.hot == Some(h);
        let mid_y = |r: &RECT| r.top + (r.bottom - r.top - lh) / 2;
        let (l, r) = (px(PAD), px(WIDTH - PAD));
        cv.clear(th.card);
        if let Some(tl) = g.layout(&w(t("icons.title")), &g.f.big, (r - l) as f32, g.f.big_lh as f32) {
            cv.text(&tl, l, px(22), th.text);
        }
        for (k, (head, choices)) in TEXT.iter().enumerate() {
            put(g, cv, t(head), &g.f.semi, l, px(SECTION[k]), th.text);
            // the chosen one last, so its accent wins the shared edges
            for pass in [false, true] {
                for (i, (label, _)) in choices.iter().enumerate() {
                    let (i, sel) = (i as u8, self.vals[k] == i as u8);
                    if sel != pass {
                        continue;
                    }
                    let rc = self.seg(k, i);
                    let (a, b) = (if i == 0 { rad(6.0) } else { 0.0 }, if i == 2 { rad(6.0) } else { 0.0 });
                    cv.corners(&rc, [a, b, b, a], if sel { th.hl } else if hot(Hot::Seg(k, i)) { th.row } else { th.card }, if sel { th.accent } else { th.line });
                    // the folder that way, over its name
                    let (p, x) = (px(PREVIEW), (rc.left + rc.right - px(PREVIEW)) / 2);
                    if let Some(Some(bm)) = self.shown.1.get(i as usize) {
                        cv.bitmap(bm, &RECT { left: x, top: rc.top + px(8), right: x + p, bottom: rc.top + px(8) + p }, 1.0);
                    }
                    let row = RECT { top: rc.bottom - px(30), ..rc };
                    put_mid(g, cv, t(label), if sel { &g.f.semi } else { &g.f.reg }, &row, mid_y(&row), if sel { th.accent } else if hot(Hot::Seg(k, i)) { th.text } else { th.muted });
                }
            }
            let hint = choices[self.vals[k].min(2) as usize].1;
            if let Some(tl) = g.layout(&w(t(hint)), &g.f.reg, (r - l) as f32, (2 * lh) as f32) {
                cv.text(&tl, l, px(SECTION[k] + 26 + SEG_H + 10), th.muted);
            }
        }
        put(g, cv, t("icons.arrow"), &g.f.semi, l, px(SECTION[1]), th.text);
        // Size: a track, filled up to the knob, a tick at Windows' own size, the value
        let tr = self.track();
        let cy = (tr.top + tr.bottom) / 2;
        let lo = *ARROW_SIZES.start() as i32;
        let at = |v: i32| tr.left + (tr.right - tr.left) * (v - lo) / (*ARROW_SIZES.end() as i32 - lo);
        put(g, cv, t("icons.arrow_size"), &g.f.reg, px(WIDTH / 2), mid_y(&tr), th.muted);
        let (kx, bar) = (at(self.size as i32), px(2));
        cv.rrect(&RECT { left: tr.left, top: cy - bar, right: tr.right, bottom: cy + bar }, bar as f32, Some(th.line), None);
        cv.rrect(&RECT { left: tr.left, top: cy - bar, right: kx, bottom: cy + bar }, bar as f32, Some(th.accent), None);
        let tick = at(100);
        cv.fill(&RECT { left: tick, top: cy + px(5), right: tick + px(1).max(1), bottom: cy + px(9) }, th.muted, 1.0);
        let big = hot(Hot::Size) || self.sliding;
        cv.circle(kx as f32, cy as f32, rad(if big { 9.0 } else { 8.0 }), th.accent, 1.0);
        cv.circle(kx as f32, cy as f32, rad(3.5), th.card, 1.0);
        let val = RECT { left: tr.right + px(8), top: tr.top, right: r, bottom: tr.bottom };
        put_mid(g, cv, &format!("{}%", self.size), &g.f.reg, &val, mid_y(&val), th.text);
        for (i, name) in ARROW_NAMES.iter().enumerate() {
            let (i, sel) = (i as u8, self.vals[1] == i as u8);
            let rc = self.swatch(i);
            cv.rrect(&rc, rad(6.0), Some(if sel { th.hl } else if hot(Hot::Arrow(i)) { th.row } else { th.card }), Some(if sel { th.accent } else { th.line }));
            if let Some(Some(b)) = self.arrows.1.get(i as usize) {
                let (a, x) = (px(ARROW), (rc.left + rc.right - px(ARROW)) / 2);
                cv.bitmap(b, &RECT { left: x, top: rc.top + px(10), right: x + a, bottom: rc.top + px(10) + a }, 1.0);
            }
            let label = if name.0.is_empty() { name.1 } else { t(name.0) };
            let row = RECT { top: rc.top + px(10 + ARROW + 4), bottom: rc.bottom - px(4), ..rc };
            put_mid(g, cv, label, if sel { &g.f.semi } else { &g.f.reg }, &row, mid_y(&row), if sel { th.accent } else { th.muted });
        }
        // footer: a rule, where it shows, Done
        let done = self.done();
        let rule = done.top - px(18);
        cv.fill(&RECT { left: 0, top: rule, right: c.right, bottom: rule + 1 }, th.line, 1.0);
        if let Some(tl) = g.layout(&w(t("icons.grid")), &g.f.reg, (done.left - px(16) - l) as f32, (2 * lh) as f32) {
            let m = super::gfx::metrics(&tl);
            cv.text(&tl, l, (done.top + done.bottom - m.height.round() as i32) / 2, th.muted);
        }
        cv.rrect(&done, rad(6.0), Some(th.accent), None);
        if hot(Hot::Done) {
            cv.rrect_a(&done, rad(6.0), Some(ink(th.accent)), None, 0.12);
        }
        put_mid(g, cv, t("apps.done"), &g.f.semi, &done, mid_y(&done), ink(th.accent));
        self.cv.end();
    }
}

/// The dialog, over `owner` (which waits, disabled, until it closes).
pub fn open(owner: HWND) {
    if with(|p| unsafe { SetForegroundWindow(p.hwnd) }).is_some() {
        return;
    }
    let Some((vals, size)) = super::with(|u| ([u.folder_icons, u.link_arrow], u.link_size)) else { return };
    static CLASS: std::sync::Once = std::sync::Once::new();
    let class = wz("MosaicIcons");
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
        let hwnd = CreateWindowExW(0, class.as_ptr(), wz(t("icons.title")).as_ptr(), WS_POPUP, x, y, w, h, owner, null_mut(), GetModuleHandleW(null()), null());
        if hwnd.is_null() {
            return;
        }
        let round: u32 = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(hwnd, 33, &round as *const _ as *const std::ffi::c_void, 4);
        let dark: BOOL = theme().dark as i32;
        DwmSetWindowAttribute(hwnd, 20, &dark as *const _ as *const std::ffi::c_void, 4);
        let d = Dlg { hwnd, owner, cv: Canvas::new(hwnd, &gfx), gfx, scale, vals, arrows: (0, Vec::new()), previews: None, shown: (0, Vec::new()), size, sliding: false, hot: None, press: None, tracking: false };
        DLG.with(|c| *c.borrow_mut() = Some(d));
        previews(hwnd, (PREVIEW as f64 * scale).round() as i32);
        EnableWindow(owner, 0);
        ShowWindow(hwnd, SW_SHOW);
        SetFocus(hwnd);
    }
}

/// A folder of pictures as Type (Pictures' own folder), Contents (a plain folder showing a photo inside) and Both
/// (Pictures' folder holding it) show it, `size` px, made off this thread and posted to `hwnd`.
fn previews(hwnd: HWND, size: i32) {
    let hw = hwnd as isize;
    super::shell::shell_spawn(Box::new(move |dropped| {
        if dropped {
            return;
        }
        let big = 256; // drawn large, made small as a whole: smooth edges
        let icon = |p: &str| super::shell::thumbnail(p, big, true).and_then(|b| png_pixels(&b, big));
        let kind = super::side::kind_dir(1).and_then(|d| icon(&d));
        let plain = icon(&std::env::temp_dir().to_string_lossy());
        let inside = |f: &Option<Px>| f.as_ref().and_then(|f| super::fold_in(f, SAMPLE, false));
        let out: Box<[Option<Px>; 3]> = Box::new([kind.clone(), inside(&plain).or(plain), inside(&kind).or(kind)].map(|p| p.map(|p| super::shrink(p, size))));
        let raw = Box::into_raw(out);
        if unsafe { PostMessageW(hw as HWND, WM_PREVIEWS, 0, raw as isize) } == 0 {
            drop(unsafe { Box::from_raw(raw) }); // closed already
        }
    }));
}

fn close() {
    let Some(p) = DLG.with(|c| c.borrow_mut().take()) else { return };
    unsafe {
        EnableWindow(p.owner, 1);
        SetForegroundWindow(p.owner);
        DestroyWindow(p.hwnd);
        if std::env::args().any(|a| a == "--icons") {
            PostMessageW(p.owner, WM_CLOSE, 0, 0);
        }
    }
}

/// Closed by a click outside it.
pub fn dismiss() {
    close();
}

pub fn hwnd() -> Option<HWND> {
    with(|p| p.hwnd)
}

unsafe extern "system" fn proc_(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_ERASEBKGND => return 1,
        WM_PREVIEWS => {
            let p = *Box::from_raw(l as *mut [Option<Px>; 3]);
            with(|d| {
                (d.previews, d.shown) = (Some(p), (0, Vec::new()));
                InvalidateRect(h, null(), 0);
            });
            return 0;
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(h, &mut ps);
            with(|p| p.paint());
            EndPaint(h, &ps);
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
                if p.sliding {
                    return p.slide(x);
                }
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
        WM_LBUTTONDOWN => {
            let (x, y) = xy(l);
            let slide = with(|p| {
                p.press = p.hit(x, y);
                p.sliding = p.press == Some(Hot::Size);
                if p.sliding {
                    p.slide(x);
                }
                p.sliding
            });
            if slide == Some(true) {
                SetCapture(h);
            }
            return 0;
        }
        WM_LBUTTONUP if with(|p| p.sliding) == Some(true) => {
            ReleaseCapture(); // → WM_CAPTURECHANGED, which saves
            return 0;
        }
        WM_CAPTURECHANGED => {
            with(|p| {
                if std::mem::take(&mut p.sliding) {
                    p.press = None;
                    p.resize(p.size, true);
                }
            });
            return 0;
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(l);
            match with(|p| p.press.take().filter(|&b| p.hit(x, y) == Some(b))).flatten() {
                Some(Hot::Done) => close(),
                Some(Hot::Seg(k, i)) => _ = with(|p| p.act(k, i)),
                Some(Hot::Arrow(i)) => _ = with(|p| p.act(1, i)),
                Some(Hot::Size) | None => {}
            }
            return 0;
        }
        WM_KEYDOWN => {
            match wp as u16 {
                VK_ESCAPE | VK_RETURN => close(),
                k @ (VK_LEFT | VK_RIGHT) => _ = with(|p| p.resize(if k == VK_LEFT { p.size.saturating_sub(10) } else { p.size + 10 }, true)),
                _ => {}
            }
            return 0;
        }
        WM_ACTIVATE if wp & 0xffff == WA_INACTIVE as usize && !super::owns(h, l as HWND) => {
            close(); // a click outside (another window, another app): gone, as a menu
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
