//! The window an update icon in the tab strip opens: what's new in the release, and what to do about it: update
//! now, be reminded later (after a time to pick), skip this version, never check, or read it on GitHub.

use std::cell::RefCell;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromWindow, ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gfx::{metrics, Bitmap, Canvas, Gfx, Text};
use super::themes::ink;
use super::{put, put_mid, t, theme, w, wz, xy};
use crate::update::{self, Info, Progress, GREEN, REMIND};

const PAD: i32 = 20;
const WIDTH: i32 = 520;
const NOTES_H: i32 = 250;
const ROW_H: i32 = 42;

#[derive(Clone, Copy, PartialEq)]
enum Hot {
    Now,
    Remind,
    Less,
    More,
    Skip,
    Never,
    Page,
}
const ROWS: [Hot; 5] = [Hot::Now, Hot::Remind, Hot::Skip, Hot::Never, Hot::Page];

struct Dlg {
    hwnd: HWND,
    owner: HWND,
    gfx: Gfx,
    cv: Canvas,
    scale: f64,
    info: Info,
    hot: Option<Hot>,
    press: Option<Hot>,
    tracking: bool,
    scroll: i32,
    remind: usize,
    gh: Option<Bitmap>, // the GitHub mark, in the accent colour
}

thread_local!(static DLG: RefCell<Option<Dlg>> = const { RefCell::new(None) });

fn with<R>(f: impl FnOnce(&mut Dlg) -> R) -> Option<R> {
    DLG.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// Where the parts start (px): the "you have" line, "What's new", its box, the options.
struct Lay {
    sub: i32,
    label: i32,
    notes: i32,
    opts: i32,
    height: i32,
}

impl Dlg {
    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.scale).round() as i32
    }
    fn width(&self) -> i32 {
        self.px(WIDTH)
    }

    fn lay(&self) -> Lay {
        let (g, p) = (&self.gfx, |v| self.px(v));
        let sub = p(18) + g.f.big_lh + p(2);
        let label = sub + g.f.lh + p(18);
        let notes = label + g.f.big_lh + p(6);
        let opts = notes + p(NOTES_H) + p(14);
        Lay { sub, label, notes, opts, height: opts + 5 * p(ROW_H + 2) + p(PAD - 6) }
    }

    fn notes_box(&self) -> RECT {
        let l = self.lay();
        rect(self.px(PAD), l.notes, self.width() - self.px(PAD), l.notes + self.px(NOTES_H))
    }

    fn row(&self, i: usize) -> RECT {
        let top = self.lay().opts + i as i32 * self.px(ROW_H + 2);
        rect(self.px(PAD) - self.px(6), top, self.width() - self.px(PAD) + self.px(6), top + self.px(ROW_H))
    }

    /// The ‹ and › around the reminder's time, and the time between them.
    fn stepper(&self) -> (RECT, RECT, RECT) {
        let r = self.row(1);
        let (s, wd) = (self.px(26), self.px(86));
        let right = r.right - self.px(10);
        let y = (r.top + r.bottom) / 2 - s / 2;
        (rect(right - 2 * s - wd, y, right - s - wd, y + s), rect(right - s - wd, y, right - s, y + s), rect(right - s, y, right, y + s))
    }

    fn hit(&self, x: i32, y: i32) -> Option<Hot> {
        let (less, _, more) = self.stepper();
        if inside(&less, x, y) {
            return Some(Hot::Less);
        }
        if inside(&more, x, y) {
            return Some(Hot::More);
        }
        (0..5).find(|&i| inside(&self.row(i), x, y)).map(|i| ROWS[i])
    }

    /// The notes as items: (is it a bullet, its text).
    fn items(&self) -> Vec<(bool, String)> {
        let n = if self.info.notes.is_empty() { t("update.no_notes").to_string() } else { self.info.notes.clone() };
        n.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| match l.strip_prefix("• ") {
                Some(r) => (true, r.to_string()),
                None => (false, l.to_string()),
            })
            .collect()
    }

    /// One item laid out (a bullet's text is indented past its dot).
    fn item_text(&self, bullet: bool, s: &str) -> Option<(Text, i32)> {
        let f = &self.gfx.f.tabs[2];
        let indent = if bullet { self.px(20) } else { 0 };
        let b = self.notes_box();
        let inner = (b.right - b.left - self.px(40) - indent) as f32;
        self.gfx.wrapped(&w(s), if bullet { &f.0 } else { &f.1 }, inner).map(|t| (t, indent))
    }

    fn notes_height(&self) -> i32 {
        self.items().iter().filter_map(|(b, s)| self.item_text(*b, s)).map(|(t, _)| metrics(&t).height.ceil() as i32 + self.px(8)).sum()
    }

    fn max_scroll(&self) -> i32 {
        (self.notes_height() + self.px(20) - self.px(NOTES_H)).max(0)
    }

    /// Do `h`; true when that leaves nothing more to decide (the caller closes the window, which it can't from in here).
    fn act(&mut self, h: Hot) -> bool {
        match h {
            Hot::Now => {
                if self.info.installer.is_empty() {
                    open_url(&self.info.page);
                } else {
                    update::reset();
                    update::install(&self.info);
                    return true; // it carries on by itself: the installer's own window follows
                }
            }
            Hot::Remind => {
                update::remind_in(REMIND[self.remind].0);
                return true;
            }
            Hot::Less | Hot::More => {
                self.remind = if h == Hot::More { (self.remind + 1) % REMIND.len() } else { (self.remind + REMIND.len() - 1) % REMIND.len() };
                let k = self.remind;
                update::set_prefs(|p| p.remind = k);
            }
            Hot::Skip => {
                update::skip(&self.info.version);
                return true;
            }
            Hot::Never => {
                update::set_prefs(|p| p.check = false);
                return true;
            }
            Hot::Page => open_url(&self.info.page),
        }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
        false
    }

    // ---- painting ----

    fn paint(&mut self) {
        let max = self.max_scroll();
        self.scroll = self.scroll.clamp(0, max);
        if !self.cv.begin() {
            return;
        }
        if self.gh.is_none() {
            self.gh = github_mark(&mut self.cv, theme().accent);
        }
        let th = theme();
        self.cv.clear(th.card);
        let (g, cv, lay) = (&self.gfx, &self.cv, self.lay());
        let (l, lh) = (self.px(PAD), g.f.lh);
        let rad = |v: f64| (v * self.scale) as f32;
        // title: the download mark in green, the version on offer, and what's running
        let d = self.px(30);
        let (bx, by) = (l + d / 2, self.px(18) + (g.f.big_lh + lh + self.px(2)) / 2);
        self.icon(Hot::Now, bx, by, GREEN);
        let tx = l + d + self.px(10);
        put(g, cv, &t("update.title").replace("{v}", &self.info.version), &g.f.big, tx, self.px(18), th.text);
        let mut have = t("update.have").replace("{c}", &update::current());
        if !self.info.date.is_empty() {
            have = format!("{have} · {}", t("update.released").replace("{d}", &self.info.date));
        }
        put(g, cv, &have, &g.f.reg, tx, lay.sub, th.muted);
        // what's new: each change on its own, a green dot before it
        put(g, cv, t("update.whats_new"), &g.f.big, l, lay.label, th.text);
        let nb = self.notes_box();
        cv.rrect(&nb, rad(8.0), Some(th.row), Some(th.line));
        cv.clip(&rect(nb.left + 1, nb.top + 1, nb.right - 1, nb.bottom - 1));
        let mut y = nb.top + self.px(12) - self.scroll;
        for (bullet, s) in self.items() {
            let Some((tl, indent)) = self.item_text(bullet, &s) else { continue };
            let x = nb.left + self.px(18);
            if bullet {
                cv.circle((x + self.px(5)) as f32, (y + g.f.tabs[2].2 / 2) as f32, rad(3.0), GREEN, 1.0);
            }
            cv.text(&tl, x + indent, y, th.text);
            y += metrics(&tl).height.ceil() as i32 + self.px(8);
        }
        cv.unclip();
        if max > 0 {
            // a thin bar at the right edge: where the text is
            let (track, total) = ((nb.bottom - nb.top - self.px(12)) as f32, (max + self.px(NOTES_H)) as f32);
            let (len, off) = (track * self.px(NOTES_H) as f32 / total, track * self.scroll as f32 / total);
            let x = nb.right - self.px(8);
            let bar = rect(x, nb.top + self.px(6) + off as i32, x + self.px(3), nb.top + self.px(6) + (off + len) as i32);
            cv.rrect(&bar, rad(1.5), Some(th.muted), None);
        }
        // the options
        let progress = update::progress();
        let tab = &g.f.tabs[2];
        for (i, &h) in ROWS.iter().enumerate() {
            let rc = self.row(i);
            let hot = self.hot == Some(h) || (h == Hot::Remind && matches!(self.hot, Some(Hot::Less | Hot::More)));
            let primary = h == Hot::Now;
            if primary {
                cv.rrect(&rc, rad(8.0), Some(GREEN), None);
                if hot && progress == Progress::Idle {
                    cv.rrect_a(&rc, rad(8.0), Some(ink(GREEN)), None, 0.12);
                }
            } else if hot {
                cv.rrect(&rc, rad(8.0), Some(th.row), None);
            }
            let name = match h {
                Hot::Now => match &progress {
                    Progress::Downloading => t("update.downloading"),
                    Progress::Installing => t("update.installing"),
                    Progress::Failed(e) => t(if e == "cancelled" { "update.cancelled" } else { "update.failed" }),
                    Progress::Idle => t("update.now"),
                },
                Hot::Remind => t("update.remind"),
                Hot::Skip => t("update.skip"),
                Hot::Never => t("update.never"),
                _ => t("update.page"),
            };
            let (cx, cy) = (rc.left + self.px(26), (rc.top + rc.bottom) / 2);
            if h == Hot::Page {
                if let Some(b) = &self.gh {
                    let s = self.px(18);
                    cv.bitmap(b, &rect(cx - s / 2, cy - s / 2, cx + s / 2, cy + s / 2), 1.0);
                }
            } else {
                self.icon(h, cx, cy, if primary { ink(GREEN) } else { th.accent });
            }
            let tx = rc.left + self.px(52);
            let avail = if h == Hot::Remind { self.stepper().0.left - tx - self.px(8) } else { rc.right - tx - self.px(12) };
            if let Some(tl) = g.layout(&w(name), &tab.1, avail as f32, tab.2 as f32) {
                cv.text(&tl, tx, rc.top + (rc.bottom - rc.top - tab.2) / 2, if primary { ink(GREEN) } else { th.text });
            }
            if h == Hot::Remind {
                let (less, mid, more) = self.stepper();
                for (rc, h) in [(less, Hot::Less), (more, Hot::More)] {
                    if self.hot == Some(h) {
                        cv.rrect(&rc, rad(6.0), Some(th.hl), None);
                    }
                    let (cx, cy, s) = ((rc.left + rc.right) as f32 / 2.0, (rc.top + rc.bottom) as f32 / 2.0, rad(3.5));
                    let k = if h == Hot::More { -1.0 } else { 1.0 };
                    cv.lines(&[(cx + k * s / 1.6, cy - s), (cx - k * s / 1.6, cy), (cx + k * s / 1.6, cy + s)], if self.hot == Some(h) { th.text } else { th.muted }, rad(1.6));
                }
                put_mid(g, cv, t(REMIND[self.remind].1), &g.f.semi, &mid, mid.top + (mid.bottom - mid.top - lh) / 2, th.text);
            }
        }
        super::frame(&self.cv, self.hwnd, self.scale);
        self.cv.end();
    }

    /// Option `h`'s icon, centred at (cx, cy).
    fn icon(&self, h: Hot, cx: i32, cy: i32, c: u32) {
        let (cv, k) = (&self.cv, self.scale as f32);
        let (x, y, wd) = (cx as f32, cy as f32, 1.7 * k);
        match h {
            // down into a tray
            Hot::Now => {
                cv.lines(&[(x, y - 7.0 * k), (x, y + 3.0 * k)], c, wd);
                cv.lines(&[(x - 4.0 * k, y - 1.0 * k), (x, y + 3.0 * k), (x + 4.0 * k, y - 1.0 * k)], c, wd);
                cv.lines(&[(x - 6.0 * k, y + 7.0 * k), (x + 6.0 * k, y + 7.0 * k)], c, wd);
            }
            // a clock
            Hot::Remind | Hot::Less | Hot::More => {
                cv.circle_line(x, y, 8.0 * k, c, wd);
                cv.lines(&[(x, y - 4.5 * k), (x, y), (x + 3.5 * k, y + 2.0 * k)], c, wd);
            }
            // on to the one after
            Hot::Skip => {
                cv.lines(&[(x - 6.0 * k, y - 5.0 * k), (x - 1.0 * k, y), (x - 6.0 * k, y + 5.0 * k)], c, wd);
                cv.lines(&[(x + 1.0 * k, y - 5.0 * k), (x + 6.0 * k, y), (x + 1.0 * k, y + 5.0 * k)], c, wd);
            }
            // a circle struck through
            Hot::Never => {
                cv.circle_line(x, y, 8.0 * k, c, wd);
                cv.lines(&[(x - 5.7 * k, y - 5.7 * k), (x + 5.7 * k, y + 5.7 * k)], c, wd);
            }
            Hot::Page => {}
        }
    }
}

/// The GitHub mark (icons/github.png, white on clear) as a bitmap tinted `c`.
fn github_mark(cv: &mut Canvas, c: u32) -> Option<Bitmap> {
    let mut r = png::Decoder::new(&include_bytes!("../icons/github.png")[..]).read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    let (cr, cg, cb) = ((c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff);
    let px: Vec<u8> = buf
        .chunks_exact(4)
        .flat_map(|p| {
            let a = p[3] as u32;
            [(cb * a / 255) as u8, (cg * a / 255) as u8, (cr * a / 255) as u8, p[3]]
        })
        .collect();
    cv.make_bitmap(&px, info.width as i32, info.height as i32)
}

fn open_url(url: &str) {
    if url.starts_with("https://") {
        unsafe { ShellExecuteW(null_mut(), wz("open").as_ptr(), wz(url).as_ptr(), null(), null(), 1) };
    }
}

/// An action that leaves nothing more to decide: the window goes and the strip loses its icon.
fn finish() {
    close();
    super::with(|u| {
        u.layout();
        u.invalidate();
    });
}

/// The update window, in the middle of `owner`'s screen, for the release on offer.
pub fn open(owner: HWND) {
    close();
    let Some(info) = update::pending() else { return };
    static CLASS: std::sync::Once = std::sync::Once::new();
    let class = wz("MosaicUpdate");
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
        let hwnd = CreateWindowExW(0, class.as_ptr(), wz("Mosaic").as_ptr(), WS_POPUP, 0, 0, 1, 1, owner, null_mut(), GetModuleHandleW(null()), null());
        if hwnd.is_null() {
            return;
        }
        let remind = update::prefs().remind.min(REMIND.len() - 1);
        let d = Dlg { hwnd, owner, cv: Canvas::new(hwnd, &gfx), gfx, scale, info, hot: None, press: None, tracking: false, scroll: 0, remind, gh: None };
        let (w, h) = (d.width(), d.lay().height);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST), &mut mi);
        let wa = mi.rcWork;
        let (x, y) = (wa.left + (wa.right - wa.left - w) / 2, wa.top + (wa.bottom - wa.top - h) / 2);
        SetWindowPos(hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        let round: u32 = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(hwnd, 33, &round as *const _ as *const std::ffi::c_void, 4);
        let dark: BOOL = theme().dark as i32;
        DwmSetWindowAttribute(hwnd, 20, &dark as *const _ as *const std::ffi::c_void, 4);
        super::outline(hwnd);
        DLG.with(|c| *c.borrow_mut() = Some(d));
        EnableWindow(owner, 0);
        ShowWindow(hwnd, SW_SHOW);
        SetFocus(hwnd);
        SetTimer(hwnd, 1, 300, None);
    }
}

fn close() {
    let Some(p) = DLG.with(|c| c.try_borrow_mut().ok().and_then(|mut c| c.take())) else { return };
    unsafe {
        EnableWindow(p.owner, 1);
        SetForegroundWindow(p.owner);
        DestroyWindow(p.hwnd);
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
        WM_TIMER => {
            if update::progress() != Progress::Idle {
                InvalidateRect(h, null(), 0);
            }
            return 0;
        }
        WM_NCHITTEST => {
            // dragged by its title
            let mut pt = POINT { x: (l & 0xffff) as i16 as i32, y: ((l >> 16) & 0xffff) as i16 as i32 };
            ScreenToClient(h, &mut pt);
            let title = with(|p| pt.y < p.lay().sub + p.gfx.f.lh).unwrap_or(false);
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
            let d = (wp >> 16) as i16 as i32;
            with(|p| {
                p.scroll = (p.scroll - d * 3 * p.gfx.f.lh / 120).clamp(0, p.max_scroll());
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
            if let Some(Some(k)) = with(|p| p.press.take().filter(|&b| p.hit(x, y) == Some(b))) {
                if with(|p| p.act(k)) == Some(true) {
                    finish();
                }
            }
            return 0;
        }
        WM_KEYDOWN => {
            match wp as u16 {
                VK_ESCAPE => close(),
                VK_RETURN => {
                    if with(|p| p.act(Hot::Now)) == Some(true) {
                        finish();
                    }
                }
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
