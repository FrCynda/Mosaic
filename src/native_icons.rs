//! View > Icons…: what a tile shows for a folder holding one kind of file (that kind's Windows folder, what's
//! inside, or both) and for a shortcut (its own icon, what it opens, or both). Saved and shown at once.

use std::cell::RefCell;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, ScreenToClient, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gfx::{Canvas, Gfx};
use super::themes::ink;
use super::{put, put_mid, t, theme, w, wz, xy};

// the layout, in CSS px from the top: two sections, each a heading, three choices and what the chosen one does
const WIDTH: i32 = 520;
const PAD: i32 = 24;
const SECTION: [i32; 2] = [70, 196];
const FOOT_TOP: i32 = 340;
const HEIGHT: i32 = FOOT_TOP + 34 + PAD;

// per section: heading, then (label, what it does) for each value
const TEXT: [(&str, [(&str, &str); 3]); 2] = [
    ("icons.folders", [("icons.type", "icons.type_hint"), ("icons.contents", "icons.contents_hint"), ("icons.both", "icons.both_hint")]),
    ("icons.links", [("icons.icon", "icons.icon_hint"), ("icons.target", "icons.target_hint"), ("icons.both", "icons.link_both_hint")]),
];

#[derive(Clone, Copy, PartialEq)]
enum Hot {
    Seg(usize, u8),
    Done,
}

struct Dlg {
    hwnd: HWND,
    owner: HWND,
    gfx: Gfx,
    cv: Canvas,
    scale: f64,
    vals: [u8; 2], // folder_icons, link_icons
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
        RECT { left: edge(i) - (i > 0) as i32, top, right: edge(i + 1), bottom: top + self.px(34) }
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
        (0..2).flat_map(|k| (0..3).map(move |i| (k, i))).find(|&(k, i)| inside(&self.seg(k, i))).map(|(k, i)| Hot::Seg(k, i))
    }

    fn act(&mut self, k: usize, v: u8) {
        if self.vals[k] == v {
            return;
        }
        self.vals[k] = v;
        super::with(|u| {
            if k == 0 {
                u.folder_icons = v;
            } else {
                u.link_icons = v;
            }
            u.icons_changed();
        });
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn paint(&mut self) {
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
                    cv.corners(&rc, [a, b, b, a], if sel { th.hl } else { th.card }, if sel { th.accent } else { th.line });
                    put_mid(g, cv, t(label), if sel { &g.f.semi } else { &g.f.reg }, &rc, mid_y(&rc), if sel { th.accent } else if hot(Hot::Seg(k, i)) { th.text } else { th.muted });
                }
            }
            let hint = choices[self.vals[k].min(2) as usize].1;
            if let Some(tl) = g.layout(&w(t(hint)), &g.f.reg, (r - l) as f32, (2 * lh) as f32) {
                cv.text(&tl, l, px(SECTION[k] + 70), th.muted);
            }
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
    let Some(vals) = super::with(|u| [u.folder_icons, u.link_icons]) else { return };
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
        let d = Dlg { hwnd, owner, cv: Canvas::new(hwnd, &gfx), gfx, scale, vals, hot: None, press: None, tracking: false };
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
            with(|p| p.press = p.hit(x, y));
            return 0;
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(l);
            match with(|p| p.press.take().filter(|&b| p.hit(x, y) == Some(b))).flatten() {
                Some(Hot::Done) => close(),
                Some(Hot::Seg(k, i)) => _ = with(|p| p.act(k, i)),
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
