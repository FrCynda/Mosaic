//! View > Theme…: a small dialog showing every theme as a miniature of the window. Picking one previews it live
//! (the whole app cross-fades to it); Apply keeps it, Cancel (Esc, or closing) goes back to the one before.

use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, ScreenToClient, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gfx::{Canvas, Gfx};
use super::{put_mid, spring, t, theme, theme_name, wz, xy, Theme, THEMES};

const COLS: usize = 4;
const CARD_W: i32 = 148; // a theme's miniature (CSS px)
const CARD_H: i32 = 86;
const NAME_H: i32 = 26; // its name, under it
const GAP: i32 = 12;
const PAD: i32 = 24;

#[derive(Clone, Copy, PartialEq)]
enum Hot {
    Card(usize),
    Apply,
    Cancel,
}

struct Picker {
    hwnd: HWND,
    owner: HWND,
    gfx: Gfx,
    cv: Canvas,
    scale: f64,
    was: String,     // the theme before: Cancel goes back to it
    sel: usize,      // the one picked (previewed)
    ring: [f64; 4],  // the selection ring, springing to the picked card
    vel: [f64; 4],
    hot: Option<Hot>,
    press: Option<Hot>,
    tracking: bool,
    born: Instant, // fades in
    tick: Instant,
}

thread_local!(static PICKER: RefCell<Option<Picker>> = const { RefCell::new(None) });

fn with<R>(f: impl FnOnce(&mut Picker) -> R) -> Option<R> {
    PICKER.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

/// Where theme `i`'s card sits (CSS px): the dark ones, then under their own heading the light ones, `COLS` a row.
fn cell(i: usize) -> (i32, i32) {
    let dark = THEMES.iter().filter(|t| t.2.dark).count();
    let (sec, k) = if i < dark { (0, i) } else { (1, i - dark) };
    let rows_before = if sec == 0 { 0 } else { dark.div_ceil(COLS) as i32 };
    let (col, row) = ((k % COLS) as i32, (k / COLS) as i32 + rows_before);
    (PAD + col * (CARD_W + GAP), 64 + (sec + 1) * 34 + sec * 8 + row * (CARD_H + NAME_H + GAP))
}

/// Text that reads on `c`: near-black on a light colour, white on a dark one.
fn ink(c: u32) -> u32 {
    let ch = |s: u32| ((c >> s) & 0xff) as f64 / 255.0;
    if 0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0) > 0.55 { 0x101216 } else { 0xffffff }
}

impl Picker {
    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.scale).round() as i32
    }
    fn size(scale: f64) -> (i32, i32) {
        let w = 2 * PAD + COLS as i32 * CARD_W + (COLS as i32 - 1) * GAP;
        let h = cell(THEMES.len() - 1).1 + CARD_H + NAME_H + 100; // the last card, its name, the footer
        ((w as f64 * scale).round() as i32, (h as f64 * scale).round() as i32)
    }
    /// Theme `i`'s miniature.
    fn card(&self, i: usize) -> RECT {
        let (x, y) = cell(i);
        RECT { left: self.px(x), top: self.px(y), right: self.px(x + CARD_W), bottom: self.px(y + CARD_H) }
    }
    /// A section's heading, above its first card.
    fn heading(&self, dark: bool) -> (i32, i32) {
        let c = self.card(THEMES.iter().position(|t| t.2.dark == dark).unwrap_or(0));
        (c.left, c.top - self.px(30))
    }
    fn buttons(&self) -> (RECT, RECT) {
        let mut c: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut c) };
        let (bw, bh) = (self.px(104), self.px(34));
        let (y, x) = (c.bottom - self.px(PAD) - bh, c.right - self.px(PAD));
        let apply = RECT { left: x - bw, top: y, right: x, bottom: y + bh };
        let cancel = RECT { left: apply.left - self.px(10) - bw, right: apply.left - self.px(10), ..apply };
        (apply, cancel)
    }
    fn hit(&self, x: i32, y: i32) -> Option<Hot> {
        let inside = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        let (a, c) = self.buttons();
        if inside(&a) {
            return Some(Hot::Apply);
        }
        if inside(&c) {
            return Some(Hot::Cancel);
        }
        (0..THEMES.len()).find(|&i| {
            let r = self.card(i);
            inside(&RECT { bottom: r.bottom + self.px(NAME_H), ..r })
        }).map(Hot::Card)
    }
    fn ring_to(&self) -> [f64; 4] {
        let (r, o) = (self.card(self.sel), self.px(4));
        [(r.left - o) as f64, (r.top - o) as f64, (r.right + o) as f64, (r.bottom + o) as f64]
    }
    fn pick(&mut self, i: usize) {
        if i == self.sel {
            return;
        }
        self.sel = i;
        super::theme_preview(THEMES[i].0);
        self.tick = Instant::now();
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }
    /// The picked theme's neighbour on screen: by `dx` along a row, `dy` rows (the sections stacked as one grid).
    fn nudge(&mut self, dx: i32, dy: i32) {
        let dark = THEMES.iter().filter(|t| t.2.dark).count();
        let (sec, k) = if self.sel < dark { (0, self.sel) } else { (1, self.sel - dark) };
        let sizes = [dark, THEMES.len() - dark];
        let base = |s: usize| if s == 0 { 0 } else { dark };
        let (mut s, mut col, mut row) = (sec, (k % COLS) as i32 + dx, (k / COLS) as i32 + dy);
        let rows = |s: usize| sizes[s].div_ceil(COLS) as i32;
        if row < 0 && s == 1 {
            (s, row) = (0, rows(0) - 1);
        } else if row >= rows(s) && s == 0 {
            (s, row) = (1, 0);
        }
        col = col.clamp(0, COLS as i32 - 1);
        row = row.clamp(0, rows(s) - 1);
        let k = ((row * COLS as i32 + col) as usize).min(sizes[s] - 1);
        self.pick(base(s) + k);
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
        cv.clear(th.card);
        // title, then the two sections' headings
        if let Some(tl) = g.layout(&super::w(&t("menu.theme").replace('…', "")), &g.f.big, (c.right - 2 * px(PAD)) as f32, g.f.big_lh as f32) {
            cv.text(&tl, px(PAD), px(22), th.text);
        }
        for dark in [true, false] {
            let (x, y) = self.heading(dark);
            super::put(g, cv, t(if dark { "theme.dark" } else { "theme.light" }), &g.f.semi, x, y, th.muted);
        }
        // the cards: each a miniature of the window in its own colours
        for (i, (_, name, p)) in THEMES.iter().enumerate() {
            let r = self.card(i);
            let hot = self.hot == Some(Hot::Card(i));
            mini(cv, &r, p, th.card, s);
            if hot && i != self.sel {
                cv.rrect(&RECT { left: r.left - px(4), top: r.top - px(4), right: r.right + px(4), bottom: r.bottom + px(4) }, rad(12.0), None, Some(th.muted));
            }
            let nr = RECT { top: r.bottom + px(6), bottom: r.bottom + px(6) + lh, ..r };
            let on = i == self.sel;
            put_mid(g, cv, name, if on { &g.f.semi } else { &g.f.reg }, &nr, nr.top, if on || hot { th.text } else { th.muted });
        }
        // the selection ring, on its way to the picked card, with a check
        let r = RECT { left: self.ring[0].round() as i32, top: self.ring[1].round() as i32, right: self.ring[2].round() as i32, bottom: self.ring[3].round() as i32 };
        cv.rrect(&r, rad(12.0), None, Some(th.accent));
        cv.rrect(&RECT { left: r.left + 1, top: r.top + 1, right: r.right - 1, bottom: r.bottom - 1 }, rad(11.0), None, Some(th.accent));
        let (cx, cy) = (r.right as f32 - rad(4.0), r.top as f32 + rad(4.0));
        cv.circle(cx, cy, rad(9.0), th.accent, 1.0);
        cv.circle_line(cx, cy, rad(9.0), th.card, rad(2.0));
        cv.check(cx, cy, rad(4.0), ink(th.accent), rad(1.6));
        // footer: a rule, then Cancel and Apply
        let (apply, cancel) = self.buttons();
        cv.fill(&RECT { left: 0, top: apply.top - px(PAD) + px(4), right: c.right, bottom: apply.top - px(PAD) + px(5) }, th.line, 1.0);
        let ty = |b: &RECT| b.top + (b.bottom - b.top - lh) / 2;
        cv.rrect(&cancel, rad(6.0), Some(if self.hot == Some(Hot::Cancel) { th.hl } else { th.card }), Some(th.line));
        put_mid(g, cv, t("theme.cancel"), &g.f.reg, &cancel, ty(&cancel), th.text);
        cv.rrect(&apply, rad(6.0), Some(th.accent), None);
        if self.hot == Some(Hot::Apply) {
            cv.rrect_a(&apply, rad(6.0), Some(ink(th.accent)), None, 0.12);
        }
        put_mid(g, cv, t("theme.apply"), &g.f.semi, &apply, ty(&apply), ink(th.accent));
        self.cv.end();
    }
}

/// Theme `p` drawn small in `r`: the tab strip with its tab, the toolbar and search box, the places panel, a few
/// rows (one selected, one a folder in the accent) and the status bar. `under` is what's around the card.
fn mini(cv: &Canvas, r: &RECT, p: &Theme, under: u32, s: f64) {
    let px = |v: f64| (v * s).round() as i32;
    let rad = |v: f64| (v * s) as f32;
    let (l, t, rr, b) = (r.left, r.top, r.right, r.bottom);
    let bar = |x0: i32, y: i32, w: i32, c: u32, a: f32| cv.rrect_a(&RECT { left: x0, top: y, right: x0 + w, bottom: y + px(4.0) }, rad(2.0), Some(c), None, a);
    cv.fill(r, p.bg, 1.0);
    // tab strip and the open tab
    let strip = t + px(14.0);
    cv.tab(&RECT { left: l + px(12.0), top: t + px(4.0), right: l + px(52.0), bottom: strip }, rad(3.0), rad(3.0), p.card, None);
    bar(l + px(18.0), t + px(7.5), px(22.0), p.text, 0.8);
    bar(l + px(60.0), t + px(7.5), px(20.0), p.muted, 0.6);
    // toolbar and search box
    let tool = strip + px(14.0);
    cv.fill(&RECT { left: l, top: strip, right: rr, bottom: tool }, p.card, 1.0);
    cv.fill(&RECT { left: l, top: tool, right: rr, bottom: tool + 1 }, p.line, 1.0);
    cv.rrect(&RECT { left: l + px(26.0), top: strip + px(3.0), right: rr - px(30.0), bottom: tool - px(3.0) }, rad(2.0), Some(p.bg), Some(p.line));
    cv.rrect(&RECT { left: rr - px(26.0), top: strip + px(3.0), right: rr - px(8.0), bottom: tool - px(3.0) }, rad(2.0), Some(p.hl), Some(p.accent));
    // status bar
    let st = b - px(9.0);
    cv.fill(&RECT { left: l, top: st, right: rr, bottom: b }, p.card, 1.0);
    cv.fill(&RECT { left: l, top: st, right: rr, bottom: st + 1 }, p.line, 1.0);
    // places panel
    let side = l + px(34.0);
    cv.fill(&RECT { left: l, top: tool + 1, right: side, bottom: st }, p.card, 1.0);
    cv.fill(&RECT { left: side - 1, top: tool + 1, right: side, bottom: st }, p.line, 1.0);
    for k in 0..3 {
        let y = tool + px(6.0 + 8.0 * k as f64);
        if k == 1 {
            cv.rrect(&RECT { left: l + px(3.0), top: y - px(2.0), right: side - px(4.0), bottom: y + px(6.0) }, rad(2.0), Some(p.hl), None);
        }
        bar(l + px(7.0), y, px(20.0), if k == 1 { p.text } else { p.muted }, 0.7);
    }
    // rows: a folder (accent), the selected one, two plain
    for k in 0..4 {
        let y = tool + px(5.0 + 9.5 * k as f64);
        if y + px(8.0) > st {
            break;
        }
        if k == 1 {
            cv.rrect(&RECT { left: side + px(4.0), top: y - px(2.5), right: rr - px(4.0), bottom: y + px(6.5) }, rad(2.0), Some(p.sel), None);
        }
        let name = if k == 0 { p.accent } else { p.text };
        bar(side + px(8.0), y, px(30.0 + 6.0 * ((k * 5) % 3) as f64), name, 0.85);
        bar(rr - px(36.0), y, px(14.0), p.muted, 0.6);
        bar(rr - px(18.0), y, px(10.0), p.muted, 0.6);
    }
    // round the corners: what's around fills them, the colours under them drawn back as a quarter circle each
    let k = px(6.0);
    for (cx, cy, inner) in [(l, t, p.bg), (rr - k, t, p.bg), (l, b - k, p.card), (rr - k, b - k, p.card)] {
        let corner = RECT { left: cx, top: cy, right: cx + k, bottom: cy + k };
        cv.clip(&corner);
        cv.fill(&corner, under, 1.0);
        let (ox, oy) = (if cx == l { l + k } else { rr - k }, if cy == t { t + k } else { b - k });
        cv.circle(ox as f32, oy as f32, k as f32, inner, 1.0);
        cv.unclip();
    }
    cv.rrect(r, k as f32, None, Some(p.line));
}

/// The dialog, over `owner` (which waits, disabled, until it closes).
pub fn open(owner: HWND) {
    if with(|p| unsafe { SetForegroundWindow(p.hwnd) }).is_some() {
        return;
    }
    static CLASS: std::sync::Once = std::sync::Once::new();
    let class = wz("MosaicThemes");
    unsafe {
        CLASS.call_once(|| {
            let mut wc: WNDCLASSEXW = std::mem::zeroed();
            wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
            wc.style = CS_DROPSHADOW | CS_DBLCLKS;
            wc.hInstance = GetModuleHandleW(null());
            wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
            wc.lpfnWndProc = Some(proc_);
            wc.lpszClassName = class.as_ptr();
            RegisterClassExW(&wc);
        });
        let scale = GetDpiForWindow(owner) as f64 / 96.0;
        let Ok(gfx) = Gfx::new(scale, crate::i18n::rtl()) else { return };
        let (w, h) = Picker::size(scale);
        let mut o: RECT = std::mem::zeroed();
        GetWindowRect(owner, &mut o);
        let (x, y) = (o.left + (o.right - o.left - w) / 2, o.top + ((o.bottom - o.top - h) / 2).max(0));
        let hwnd = CreateWindowExW(WS_EX_LAYERED,class.as_ptr(), wz(&t("menu.theme").replace('…', "")).as_ptr(), WS_POPUP, x, y, w, h, owner, null_mut(), GetModuleHandleW(null()), null());
        if hwnd.is_null() {
            return;
        }
        SetLayeredWindowAttributes(hwnd, 0, 0, LWA_ALPHA);
        let round: u32 = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(hwnd, 33, &round as *const _ as *const std::ffi::c_void, 4);
        let dark: BOOL = theme().dark as i32;
        DwmSetWindowAttribute(hwnd, 20, &dark as *const _ as *const std::ffi::c_void, 4);
        let sel = THEMES.iter().position(|t| t.0 == theme_name()).unwrap_or(0);
        let mut p = Picker { hwnd, owner, cv: Canvas::new(hwnd, &gfx), gfx, scale, was: theme_name(), sel, ring: [0.0; 4], vel: [0.0; 4], hot: None, press: None, tracking: false, born: Instant::now(), tick: Instant::now() };
        p.ring = p.ring_to();
        PICKER.with(|c| *c.borrow_mut() = Some(p));
        EnableWindow(owner, 0);
        ShowWindow(hwnd, SW_SHOW);
        SetFocus(hwnd);
    }
}

/// Close the dialog: keeping the theme picked, or going back to the one before.
fn close(keep: bool) {
    let Some(p) = PICKER.with(|c| c.borrow_mut().take()) else { return };
    unsafe {
        EnableWindow(p.owner, 1);
        SetForegroundWindow(p.owner);
        DestroyWindow(p.hwnd);
    }
    if keep {
        super::theme_apply(THEMES[p.sel].0);
    } else if THEMES[p.sel].0 != p.was {
        super::theme_preview(&p.was);
    }
}

/// Whether the dialog has a frame to draw (fading in, its ring moving, the app's colours changing under it).
pub fn moving() -> bool {
    with(|p| p.born.elapsed().as_secs_f64() < 0.16 || p.ring != p.ring_to() || p.vel != [0.0; 4]).unwrap_or(false)
}

/// One frame of its motion.
pub fn step() {
    with(|p| {
        let now = Instant::now();
        let dt = (now - p.tick).as_secs_f64().min(1.0 / 30.0);
        p.tick = now;
        let to = p.ring_to();
        for ((x, v), to) in p.ring.iter_mut().zip(p.vel.iter_mut()).zip(to) {
            spring(x, v, to, dt, 26.0, 0.3);
        }
        let k = (p.born.elapsed().as_secs_f64() / 0.16).min(1.0);
        let a = 1.0 - (1.0 - k) * (1.0 - k);
        unsafe {
            SetLayeredWindowAttributes(p.hwnd, 0, (a * 255.0).round() as u8, LWA_ALPHA);
            InvalidateRect(p.hwnd, null(), 0);
        }
    });
}

/// Repaint (the app's colours changed).
pub fn invalidate() {
    with(|p| unsafe {
        let dark: BOOL = theme().dark as i32;
        DwmSetWindowAttribute(p.hwnd, 20, &dark as *const _ as *const std::ffi::c_void, 4);
        InvalidateRect(p.hwnd, null(), 0)
    });
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
            let title = with(|p| pt.y < p.px(56)).unwrap_or(false);
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
            // a card is picked on the press: its preview starts at once
            with(|p| {
                p.press = p.hit(x, y);
                if let Some(Hot::Card(i)) = p.press {
                    p.pick(i);
                }
            });
            return 0;
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = xy(l);
            if let Some(Some(Hot::Card(_))) = with(|p| p.hit(x, y)) {
                close(true);
            }
            return 0;
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(l);
            match with(|p| p.press.take().filter(|&b| p.hit(x, y) == Some(b))).flatten() {
                Some(Hot::Apply) => close(true),
                Some(Hot::Cancel) => close(false),
                _ => {}
            }
            return 0;
        }
        WM_KEYDOWN => {
            match wp as u16 {
                VK_ESCAPE => close(false),
                VK_RETURN => close(true),
                VK_LEFT => _ = with(|p| p.nudge(-1, 0)),
                VK_RIGHT => _ = with(|p| p.nudge(1, 0)),
                VK_UP => _ = with(|p| p.nudge(0, -1)),
                VK_DOWN => _ = with(|p| p.nudge(0, 1)),
                _ => {}
            }
            return 0;
        }
        WM_CLOSE => {
            close(false);
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}
