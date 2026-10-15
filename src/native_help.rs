//! A search switch right-clicked: what it does, shown on names it would and wouldn't find with it off and on, and
//! the switch itself. Timestamps instead: a date filter to pick (which date; on, before, after or between; a period
//! or a calendar's days), written into the search box as `dm:`/`dc:`/`da:`.

use std::cell::RefCell;
use std::ptr::{null, null_mut};

use chrono::{Datelike, Days, Local, Months, NaiveDate};
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::Globalization::{GetDateFormatEx, GetLocaleInfoEx};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_NEXT, VK_PRIOR, VK_RETURN};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gfx::{metrics, Canvas, Gfx};
use super::themes::ink;
use super::{put, put_mid, t, theme, w, wz, xy, Hit};

const PAD: i32 = 20;
const HELP_W: i32 = 440;
const DATES_W: i32 = 360;

/// A switch's examples: (search, a file's name, what else it holds or "", found with the switch off, on).
type Example = (&'static str, &'static str, (&'static str, &'static str), bool, bool);

fn examples(h: Hit) -> &'static [Example] {
    const NONE: (&str, &str) = ("", "");
    match h {
        Hit::MatchCase => &[("Report", "report.pdf", NONE, true, false), ("Report", "Report 2026.pdf", NONE, true, true)],
        Hit::WholeWord => &[("cat", "concatenate.txt", NONE, true, false), ("cat", "cat photo.jpg", NONE, true, true)],
        Hit::Regex => &[(r"^IMG_\d+\.jpg$", "IMG_0042.jpg", NONE, false, true), ("a.c", "abc.txt", NONE, false, true)],
        Hit::Diacritics => &[("cafe", "Café menu.pdf", NONE, false, true), ("resume", "Résumé.docx", NONE, false, true), ("cafe", "cafe.txt", NONE, true, true)],
        Hit::Content => &[("invoice", "notes.txt", ("help.contains", "…the invoice is due Friday…"), false, true), ("invoice", "invoice-2026.pdf", NONE, true, true)],
        _ => &[("canon", "IMG_0042.jpg", ("help.camera", "Canon EOS R6"), false, true), ("beatles", "track01.mp3", ("help.artist", "The Beatles"), false, true)],
    }
}

fn title(h: Hit) -> &'static str {
    t(match h {
        Hit::Tags => "toolbar.metadata",
        Hit::Content => "toolbar.content",
        Hit::MatchCase => "toolbar.match_case",
        Hit::WholeWord => "toolbar.whole_word",
        Hit::Regex => "toolbar.regex",
        Hit::Diacritics => "toolbar.diacritics",
        _ => "help.dates",
    })
}

fn tip(h: Hit) -> &'static str {
    t(match h {
        Hit::Tags => "tip.tags",
        Hit::Content => "tip.content",
        Hit::MatchCase => "tip.match_case",
        Hit::WholeWord => "tip.whole_word",
        Hit::Regex => "tip.regex",
        _ => "tip.diacritics",
    })
}

// the periods search knows by name: (keyword, label)
const PERIODS: [(&str, &str); 8] = [
    ("today", "help.today"),
    ("yesterday", "help.yesterday"),
    ("thisweek", "help.this_week"),
    ("lastweek", "help.last_week"),
    ("thismonth", "help.this_month"),
    ("lastmonth", "help.last_month"),
    ("thisyear", "help.this_year"),
    ("lastyear", "help.last_year"),
];
const FIELDS: [(&str, &str); 3] = [("dm:", "col.modified"), ("dc:", "help.created"), ("da:", "help.accessed")];
const MODES: [&str; 4] = ["help.on_day", "help.before", "help.after", "help.between"];

/// The days `PERIODS[k]` covers, first and last, as search reads them (weeks from Monday).
fn period(k: usize, today: NaiveDate) -> (NaiveDate, NaiveDate) {
    let day = |n: u64| today - Days::new(n);
    let monday = day(today.weekday().num_days_from_monday() as u64);
    let month = today.with_day(1).unwrap_or(today);
    let year = NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap_or(today);
    let before = |d: NaiveDate| d - Days::new(1);
    match k {
        0 => (today, today),
        1 => (day(1), day(1)),
        2 => (monday, monday + Days::new(6)),
        3 => (monday - Days::new(7), before(monday)),
        4 => (month, before(month + Months::new(1))),
        5 => (month - Months::new(1), before(month)),
        6 => (year, before(year + Months::new(12))),
        _ => (year - Months::new(12), before(year)),
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pick {
    Day(NaiveDate),
    Month(NaiveDate), // its first day
    Period(usize),
    Range(NaiveDate, Option<NaiveDate>),
}

/// The filter for `field` (an index into `FIELDS`) in `mode` (on, before, after, between): `dm:2026-10-15`,
/// `dc:<thisweek`, `da:2026-10-01..2026-10-15`...
fn token(field: usize, mode: u8, pick: Pick) -> String {
    let op = match mode {
        1 => "<",
        2 => ">",
        _ => "",
    };
    let day = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    let v = match pick {
        Pick::Day(d) | Pick::Range(d, None) => format!("{op}{}", day(d)),
        Pick::Month(m) => format!("{op}{}", m.format("%Y-%m")),
        Pick::Period(k) => format!("{op}{}", PERIODS[k].0),
        Pick::Range(a, Some(b)) => format!("{}..{}", day(a.min(b)), day(a.max(b))),
    };
    format!("{}{v}", FIELDS[field].0)
}

/// `q` without its `prefix` filters (`dm:`...), with `add` at the end instead.
fn with_filter(q: &str, prefix: &str, add: Option<&str>) -> String {
    let mut out: Vec<&str> = q.split(' ').filter(|s| !s.to_lowercase().starts_with(prefix)).collect();
    while out.last() == Some(&"") {
        out.pop();
    }
    out.extend(add);
    out.join(" ").trim_start().to_string()
}

/// A locale's string (`what` a LOCALE_ constant) for the app's language, or the user's when Windows lacks it.
unsafe fn locale(what: u32, user: bool) -> String {
    let mut b = [0u16; 80];
    let lang = wz(&crate::i18n::lang());
    let mut n = if user { 0 } else { GetLocaleInfoEx(lang.as_ptr(), what, b.as_mut_ptr(), 80) };
    if n <= 1 {
        n = GetLocaleInfoEx(null(), what, b.as_mut_ptr(), 80);
    }
    String::from_utf16_lossy(&b[..(n.max(1) - 1) as usize])
}

/// "October 2026", as the app's language writes a month.
unsafe fn month_name(m: NaiveDate) -> String {
    let st = SYSTEMTIME { wYear: m.year() as u16, wMonth: m.month() as u16, wDayOfWeek: 0, wDay: 1, wHour: 0, wMinute: 0, wSecond: 0, wMilliseconds: 0 };
    let mut b = [0u16; 80];
    let lang = wz(&crate::i18n::lang());
    let mut n = GetDateFormatEx(lang.as_ptr(), 8, &st, null(), b.as_mut_ptr(), 80, null()); // DATE_YEARMONTH
    if n <= 1 {
        n = GetDateFormatEx(null(), 8, &st, null(), b.as_mut_ptr(), 80, null());
    }
    let s = String::from_utf16_lossy(&b[..(n.max(1) - 1) as usize]);
    let mut c = s.chars();
    c.next().map_or(s.clone(), |f| f.to_uppercase().chain(c).collect()) // "octobre 2026" → "Octobre 2026"
}

struct Dates {
    field: usize,
    mode: u8,
    month: NaiveDate, // the first of the month shown
    pick: Option<Pick>,
    today: NaiveDate,
    first: u32,              // the week's first day, 0 Monday (the user's own setting)
    days: [String; 7],       // their shortest names, from Monday
    title: (NaiveDate, String), // the month shown, said
}

#[derive(Clone, Copy, PartialEq)]
enum Hot {
    Switch,
    Field(usize),
    Mode(u8),
    Period(usize),
    Prev,
    Next,
    Month,
    Day(NaiveDate),
    Add,
    Clear,
}

struct Dlg {
    hwnd: HWND,
    owner: HWND,
    gfx: Gfx,
    cv: Canvas,
    scale: f64,
    what: Hit,
    dates: Option<Dates>,
    hot: Option<Hot>,
    press: Option<Hot>,
    tracking: bool,
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

/// Where the help's parts start (px): the description, the note, the examples' heading, their first row; and its height.
struct HelpLay {
    desc: i32,
    note: i32,
    head: i32,
    rows: i32,
    height: i32,
}

impl Dlg {
    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.scale).round() as i32
    }
    fn width(&self) -> i32 {
        self.px(if self.dates.is_some() { DATES_W } else { HELP_W })
    }

    /// The switch, right of the title.
    fn switch(&self) -> RECT {
        let (r, cy) = (self.width() - self.px(PAD), self.px(18) + self.gfx.f.big_lh / 2);
        if self.dates.is_some() {
            // Dates: in the footer, before "track created and accessed times"
            let y = self.px(DATE_TRACK);
            return rect(self.px(PAD), y, self.px(PAD + 36), y + self.px(20));
        }
        rect(r - self.px(36), cy - self.px(10), r, cy + self.px(10))
    }

    fn help_lay(&self) -> HelpLay {
        let (g, inner) = (&self.gfx, (self.width() - 2 * self.px(PAD)) as f32);
        let tall = |s: &str, f| g.wrapped(&w(s), f, inner).map_or(0, |tl| metrics(&tl).height.ceil() as i32);
        let desc = self.px(18) + g.f.big_lh + self.px(10);
        let note = desc + tall(tip(self.what), &g.f.reg) + self.px(8);
        let head = note + if self.what == Hit::Regex { tall(t("help.regex_note"), &g.f.reg) + self.px(14) } else { self.px(6) };
        let rows = head + g.f.lh + self.px(10) + self.px(20);
        let height = rows + examples(self.what).iter().map(|e| self.row_h(e)).sum::<i32>() + self.px(PAD - 4);
        HelpLay { desc, note, head, rows, height }
    }

    fn row_h(&self, e: &Example) -> i32 {
        self.px(if e.2 .0.is_empty() { 36 } else { 48 })
    }

    fn hit(&self, x: i32, y: i32) -> Option<Hot> {
        if inside(&self.switch(), x, y) || self.dates.is_some() && inside(&self.track_label(), x, y) {
            return Some(Hot::Switch);
        }
        let d = self.dates.as_ref()?;
        let found = [(Hot::Add, self.add_r()), (Hot::Clear, self.clear_r()), (Hot::Prev, self.nav(false)), (Hot::Next, self.nav(true)), (Hot::Month, self.month_r())]
            .into_iter()
            .find(|(_, r)| inside(r, x, y))
            .map(|h| h.0);
        found
            .or_else(|| (0..3).find(|&i| inside(&self.seg(DATE_FIELD, i, 3), x, y)).map(Hot::Field))
            .or_else(|| (0..4).find(|&i| inside(&self.seg(DATE_MODE, i, 4), x, y)).map(|i| Hot::Mode(i as u8)))
            .or_else(|| (0..PERIODS.len()).find(|&i| inside(&self.chip(i), x, y)).map(Hot::Period))
            .or_else(|| (0..42).find(|&i| inside(&self.cell(i), x, y)).map(|i| Hot::Day(self.cell_day(d, i))))
    }

    // ---- the date filter ----

    /// Segment `i` of `n` in the row at `top`.
    fn seg(&self, top: i32, i: usize, n: usize) -> RECT {
        let (l, r) = (self.px(PAD), self.width() - self.px(PAD));
        let edge = |i: i32| l + (r - l) * i / n as i32;
        let i = i as i32;
        rect(edge(i) - (i > 0) as i32, self.px(top), edge(i + 1), self.px(top + 30))
    }

    fn chip(&self, k: usize) -> RECT {
        let (l, r, gap) = (self.px(PAD), self.width() - self.px(PAD), self.px(6));
        let (col, row) = ((k % 4) as i32, (k / 4) as i32);
        let edge = |i: i32| l + (r - l + gap) * i / 4;
        let top = self.px(DATE_CHIPS + row * 32);
        rect(edge(col), top, edge(col + 1) - gap, top + self.px(26))
    }

    fn nav(&self, next: bool) -> RECT {
        let top = self.px(DATE_CAL);
        let (l, r) = (self.px(PAD), self.width() - self.px(PAD));
        let s = self.px(28);
        if next {
            rect(r - s, top, r, top + s)
        } else {
            rect(l, top, l + s, top + s)
        }
    }

    fn month_r(&self) -> RECT {
        let (g, top) = (&self.gfx, self.px(DATE_CAL));
        let label = self.dates.as_ref().map_or(String::new(), |d| d.title.1.clone());
        let half = g.width(&label, &g.f.semi) / 2 + self.px(10);
        let mid = self.width() / 2;
        rect(mid - half, top, mid + half, top + self.px(28))
    }

    /// Day cell `i` (row-major, 6 rows of 7).
    fn cell(&self, i: usize) -> RECT {
        let (l, r) = (self.px(PAD), self.width() - self.px(PAD));
        let (col, row) = ((i % 7) as i32, (i / 7) as i32);
        let edge = |c: i32| l + (r - l) * c / 7;
        let top = self.px(DATE_GRID + row * 32);
        rect(edge(col), top, edge(col + 1), top + self.px(32))
    }

    fn cell_day(&self, d: &Dates, i: usize) -> NaiveDate {
        let lead = (d.month.weekday().num_days_from_monday() + 7 - d.first) % 7;
        d.month - Days::new(lead as u64) + Days::new(i as u64)
    }

    fn token_r(&self) -> RECT {
        rect(self.px(PAD), self.px(DATE_TOKEN), self.width() - self.px(PAD), self.px(DATE_TOKEN + 34))
    }

    fn track_label(&self) -> RECT {
        let s = self.switch();
        rect(s.right, s.top, self.width() - self.px(PAD), s.bottom)
    }

    fn add_r(&self) -> RECT {
        let g = &self.gfx;
        let r = self.width() - self.px(PAD);
        let w = (g.width(t("help.add"), &g.f.semi) + self.px(32)).max(self.px(104));
        rect(r - w, self.px(DATE_FOOT), r, self.px(DATE_FOOT + 32))
    }

    fn clear_r(&self) -> RECT {
        let g = &self.gfx;
        let r = self.add_r().left - self.px(8);
        let w = g.width(t("help.clear"), &g.f.reg) + self.px(28);
        rect(r - w, self.px(DATE_FOOT), r, self.px(DATE_FOOT + 32))
    }

    /// The days to light: the pick's (the range so far up to the hovered day, while its end is being chosen).
    fn lit(&self, d: &Dates) -> Option<(NaiveDate, NaiveDate)> {
        let span = |a: NaiveDate, b: NaiveDate| (a.min(b), a.max(b));
        Some(match d.pick? {
            Pick::Day(x) => (x, x),
            Pick::Month(m) => (m, m + Months::new(1) - Days::new(1)),
            Pick::Period(k) => period(k, d.today),
            Pick::Range(a, Some(b)) => span(a, b),
            Pick::Range(a, None) => match self.hot {
                Some(Hot::Day(h)) => span(a, h),
                _ => (a, a),
            },
        })
    }

    fn act(&mut self, h: Hot) {
        let Some(d) = self.dates.as_mut() else {
            if h == Hot::Switch {
                super::with(|u| u.flip(self.what));
            }
            return;
        };
        match h {
            Hot::Switch => _ = super::with(|u| u.flip(Hit::Timestamps)),
            Hot::Field(i) => d.field = i,
            Hot::Mode(m) => {
                d.pick = match (d.pick, m == 3) {
                    (Some(Pick::Day(x)), true) => Some(Pick::Range(x, None)),
                    (Some(Pick::Range(a, _)), false) => Some(Pick::Day(a)),
                    (Some(Pick::Month(_) | Pick::Period(_)), true) => None,
                    (p, _) => p,
                };
                d.mode = m;
            }
            Hot::Period(k) => {
                if d.mode == 3 {
                    d.mode = 0;
                }
                d.pick = Some(Pick::Period(k));
            }
            Hot::Prev | Hot::Next => self.turn(if h == Hot::Next { 1 } else { -1 }),
            Hot::Month if d.mode != 3 => d.pick = Some(Pick::Month(d.month)),
            Hot::Month => {}
            Hot::Day(x) => {
                d.pick = Some(match (d.mode, d.pick) {
                    (3, Some(Pick::Range(a, None))) => Pick::Range(a, Some(x)),
                    (3, _) => Pick::Range(x, None),
                    _ => Pick::Day(x),
                });
                if x.month() != d.month.month() {
                    d.month = x.with_day(1).unwrap_or(x); // a day of the month before or after: that month
                }
            }
            Hot::Add => {
                if let Some(p) = d.pick {
                    put_filter(FIELDS[d.field].0, Some(&token(d.field, d.mode, p)));
                    return close();
                }
            }
            Hot::Clear => {
                put_filter(FIELDS[d.field].0, None);
                d.pick = None;
            }
        }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    /// The calendar `n` months on.
    fn turn(&mut self, n: i32) {
        if let Some(d) = self.dates.as_mut() {
            let m = if n > 0 { d.month.checked_add_months(Months::new(n as u32)) } else { d.month.checked_sub_months(Months::new(-n as u32)) };
            d.month = m.unwrap_or(d.month);
            unsafe { InvalidateRect(self.hwnd, null(), 0) };
        }
    }

    // ---- painting ----

    fn paint(&mut self) {
        if let Some(d) = self.dates.as_mut() {
            if d.title.0 != d.month {
                d.title = (d.month, unsafe { month_name(d.month) });
            }
        }
        if !self.cv.begin() {
            return;
        }
        let th = theme();
        self.cv.clear(th.card);
        let (g, cv) = (&self.gfx, &self.cv);
        let l = self.px(PAD);
        let title_w = (self.width() - 2 * l - if self.dates.is_some() { 0 } else { self.px(48) }) as f32;
        if let Some(tl) = g.layout(&w(title(self.what)), &g.f.big, title_w, g.f.big_lh as f32) {
            cv.text(&tl, l, self.px(18), th.text);
        }
        let on = super::with(|u| u.toggled(if self.dates.is_some() { Hit::Timestamps } else { self.what })).unwrap_or(false);
        self.paint_switch(on);
        if self.dates.is_some() {
            self.paint_dates(on);
        } else {
            self.paint_help(on);
        }
        super::frame(&self.cv, self.hwnd, self.scale);
        self.cv.end();
    }

    fn paint_switch(&self, on: bool) {
        let (cv, th, r) = (&self.cv, theme(), self.switch());
        let hot = self.hot == Some(Hot::Switch);
        let rad = (r.bottom - r.top) as f32 / 2.0;
        if on {
            cv.rrect(&r, rad, Some(th.accent), None);
        } else {
            cv.rrect(&r, rad, Some(if hot { th.row } else { th.card }), Some(th.muted));
        }
        let k = rad - self.px(5) as f32 + if hot { 1.0 } else { 0.0 };
        let cx = if on { r.right as f32 - rad } else { r.left as f32 + rad };
        cv.circle(cx, (r.top + r.bottom) as f32 / 2.0, k, if on { ink(th.accent) } else { th.muted }, 1.0);
    }

    fn paint_help(&self, on: bool) {
        let (g, cv, th, lh) = (&self.gfx, &self.cv, theme(), self.gfx.f.lh);
        let (l, r) = (self.px(PAD), self.width() - self.px(PAD));
        let lay = self.help_lay();
        let wrap = |s: &str, y: i32, c: u32| {
            if let Some(tl) = g.wrapped(&w(s), &g.f.reg, (r - l) as f32) {
                cv.text(&tl, l, y, c);
            }
        };
        wrap(tip(self.what), lay.desc, th.text);
        if self.what == Hit::Regex {
            wrap(t("help.regex_note"), lay.note, th.muted);
        }
        put(g, cv, t("help.examples"), &g.f.semi, l, lay.head, th.text);
        // columns: what's searched, the file, found with the switch off, on (the one in force stands out)
        let (file_x, off_x, on_x) = (l + self.px(150), r - self.px(96), r - self.px(34));
        let col = |s: &str, x: i32, now: bool| {
            let f = if now { &g.f.semi } else { &g.f.small };
            put(g, cv, s, f, x - g.width(s, f) / 2, lay.rows - self.px(20), if now { th.accent } else { th.muted });
        };
        put(g, cv, t("help.search"), &g.f.small, l, lay.rows - self.px(20), th.muted);
        put(g, cv, t("help.file"), &g.f.small, file_x, lay.rows - self.px(20), th.muted);
        col(t("help.off"), off_x, !on);
        col(t("help.on"), on_x, on);
        let now_x = if on { on_x } else { off_x };
        let band = self.px(26);
        let all = examples(self.what).iter().map(|e| self.row_h(e)).sum::<i32>();
        cv.rrect_a(&rect(now_x - band, lay.rows - self.px(24), now_x + band, lay.rows + all), self.px(6) as f32, Some(th.accent), None, 0.08);
        let mut y = lay.rows;
        for e in examples(self.what) {
            let h = self.row_h(e);
            cv.fill(&rect(l, y, r, y + 1), th.line, 1.0);
            let detail = !e.2 .0.is_empty();
            let ty = y + (h - if detail { 2 * lh } else { lh }) / 2;
            // the search, as typed, in a box of its own
            let qw = g.width(e.0, &g.f.reg).min(file_x - l - self.px(28));
            let q = rect(l, y + (h - lh) / 2 - self.px(4), l + qw + self.px(16), y + (h + lh) / 2 + self.px(4));
            cv.rrect(&q, self.px(4) as f32, Some(th.row), Some(th.line));
            if let Some(tl) = g.layout(&w(e.0), &g.f.reg, qw as f32 + 1.0, lh as f32) {
                cv.text(&tl, q.left + self.px(8), (h - lh) / 2 + y, th.text);
            }
            if let Some(tl) = g.layout(&w(e.1), &g.f.reg, (off_x - band - file_x - self.px(4)) as f32, lh as f32) {
                cv.text(&tl, file_x, ty, th.text);
            }
            if detail {
                let s = t(e.2 .0).replace("{t}", e.2 .1);
                if let Some(tl) = g.layout(&w(&s), &g.f.small, (off_x - band - file_x - self.px(4)) as f32, lh as f32) {
                    cv.text(&tl, file_x, ty + lh, th.muted);
                }
            }
            let cy = (y + h / 2) as f32;
            for (x, found, now) in [(off_x, e.3, !on), (on_x, e.4, on)] {
                let c = if !now { th.muted } else if found { th.accent } else { th.text };
                if found {
                    cv.check(x as f32, cy, self.px(6) as f32, c, self.px(2) as f32);
                } else {
                    cv.cross(x as f32, cy, self.px(4) as f32, c, self.px(2) as f32);
                }
            }
            y += h;
        }
    }

    fn paint_dates(&self, tracking: bool) {
        let Some(d) = self.dates.as_ref() else { return };
        let (g, cv, th, lh) = (&self.gfx, &self.cv, theme(), self.gfx.f.lh);
        let rad = |v: f64| (v * self.scale) as f32;
        let hot = |h: Hot| self.hot == Some(h);
        let mid_y = |r: &RECT| r.top + (r.bottom - r.top - lh) / 2;
        // which date, then on / before / after / between: segments, the chosen last so its accent wins the shared edges
        let segs = |top: i32, labels: &[&str], sel: usize, hot_of: &dyn Fn(usize) -> Hot| {
            for pass in [false, true] {
                for (i, label) in labels.iter().enumerate() {
                    if (i == sel) != pass {
                        continue;
                    }
                    let rc = self.seg(top, i, labels.len());
                    let (a, b) = (if i == 0 { rad(6.0) } else { 0.0 }, if i + 1 == labels.len() { rad(6.0) } else { 0.0 });
                    let on = i == sel;
                    cv.corners(&rc, [a, b, b, a], if on { th.hl } else if hot(hot_of(i)) { th.row } else { th.card }, if on { th.accent } else { th.line });
                    put_mid(g, cv, label, if on { &g.f.semi } else { &g.f.reg }, &rc, mid_y(&rc), if on { th.accent } else if hot(hot_of(i)) { th.text } else { th.muted });
                }
            }
        };
        let fields: Vec<&str> = FIELDS.iter().map(|f| t(f.1)).collect();
        segs(DATE_FIELD, &fields, d.field, &Hot::Field);
        let modes: Vec<&str> = MODES.iter().map(|m| t(m)).collect();
        segs(DATE_MODE, &modes, d.mode as usize, &|i| Hot::Mode(i as u8));
        // the named periods: pills
        for (k, (_, label)) in PERIODS.iter().enumerate() {
            let rc = self.chip(k);
            let on = d.pick == Some(Pick::Period(k));
            let half = (rc.bottom - rc.top) as f32 / 2.0;
            cv.rrect(&rc, half, Some(if on { th.accent } else if hot(Hot::Period(k)) { th.row } else { th.card }), Some(if on { th.accent } else { th.line }));
            let f = if g.width(t(label), &g.f.reg) > rc.right - rc.left - self.px(10) { &g.f.small } else { &g.f.reg };
            let tl = g.layout(&w(t(label)), f, (rc.right - rc.left - self.px(6)) as f32, lh as f32);
            if let Some(tl) = tl {
                let m = metrics(&tl);
                let x = rc.left + (rc.right - rc.left - m.width.ceil() as i32) / 2;
                cv.text(&tl, x, rc.top + (rc.bottom - rc.top - m.height.round() as i32) / 2, if on { ink(th.accent) } else { th.text });
            }
        }
        // the month: ‹ October 2026 › (the name picks the whole month)
        for next in [false, true] {
            let rc = self.nav(next);
            let h = if next { Hot::Next } else { Hot::Prev };
            if hot(h) {
                cv.rrect(&rc, rad(6.0), Some(th.row), None);
            }
            let (cx, cy, s) = ((rc.left + rc.right) as f32 / 2.0, (rc.top + rc.bottom) as f32 / 2.0, rad(4.0));
            let k = if next { -1.0 } else { 1.0 };
            cv.lines(&[(cx + k * s / 1.6, cy - s), (cx - k * s / 1.6, cy), (cx + k * s / 1.6, cy + s)], if hot(h) { th.text } else { th.muted }, rad(1.6));
        }
        let mr = self.month_r();
        let month_on = d.pick == Some(Pick::Month(d.month));
        if month_on || hot(Hot::Month) && d.mode != 3 {
            cv.rrect(&mr, rad(6.0), Some(if month_on { th.hl } else { th.row }), month_on.then_some(th.accent));
        }
        put_mid(g, cv, &d.title.1, &g.f.semi, &mr, mid_y(&mr), if month_on { th.accent } else { th.text });
        // the week's days, then the grid
        for c in 0..7 {
            let rc = self.cell(c);
            let row = rect(rc.left, self.px(DATE_GRID - 22), rc.right, self.px(DATE_GRID - 4));
            put_mid(g, cv, &d.days[((c as u32 + d.first) % 7) as usize], &g.f.small, &row, row.top, th.muted);
        }
        let lit = self.lit(d);
        let ends = match d.pick {
            Some(Pick::Day(x)) => Some((x, x)),
            Some(Pick::Range(..)) => lit,
            _ => None,
        };
        let disk = rad(14.0);
        for i in 0..42 {
            let (rc, day) = (self.cell(i), self.cell_day(d, i));
            let (cx, cy) = ((rc.left + rc.right) as f32 / 2.0, (rc.top + rc.bottom) as f32 / 2.0);
            let ours = day.month() == d.month.month();
            let in_lit = lit.is_some_and(|(a, b)| day >= a && day <= b);
            let end = ends.is_some_and(|(a, b)| day == a || day == b);
            if in_lit && !end || in_lit && ends.is_some_and(|(a, b)| a != b) {
                // the band: through the cell, rounded off at a row's or the range's ends
                let (first, last) = (i % 7 == 0 || lit.is_some_and(|(a, _)| day == a), i % 7 == 6 || lit.is_some_and(|(_, b)| day == b));
                let band = rect(if first { (cx - disk) as i32 } else { rc.left }, (cy - disk) as i32, if last { (cx + disk) as i32 } else { rc.right }, (cy + disk) as i32);
                cv.corners(&band, [if first { disk } else { 0.0 }, if last { disk } else { 0.0 }, if last { disk } else { 0.0 }, if first { disk } else { 0.0 }], th.hl, th.hl);
            }
            if end {
                cv.circle(cx, cy, disk, th.accent, 1.0);
            } else if hot(Hot::Day(day)) {
                cv.circle(cx, cy, disk, th.row, 1.0);
            }
            if day == d.today && !end {
                cv.circle_line(cx, cy, disk - rad(0.5), th.accent, rad(1.2));
            }
            let c = if end { ink(th.accent) } else if in_lit { th.accent } else if ours { th.text } else { th.muted };
            let f = if day == d.today || end { &g.f.semi } else { &g.f.reg };
            let s = day.day().to_string();
            if let Some(tl) = g.layout(&w(&s), f, 1e4, 1e4) {
                let wd = metrics(&tl).width;
                cv.text_a(&tl, (cx - wd / 2.0).round() as i32, (cy as i32) - lh / 2, c, if ours || end || in_lit { 1.0 } else { 0.55 });
            }
        }
        // what goes in the search box
        let tr = self.token_r();
        cv.rrect(&tr, rad(6.0), Some(th.row), Some(th.line));
        let (s, c) = match d.pick {
            Some(p) => (token(d.field, d.mode, p), th.text),
            None => (t(if d.mode == 3 { "help.pick_range" } else { "help.pick" }).to_string(), th.muted),
        };
        put_mid(g, cv, &s, if d.pick.is_some() { &g.f.semi } else { &g.f.reg }, &tr, mid_y(&tr), c);
        // tracking created and accessed times (the Timestamps switch): wanted for those two
        let need = d.field != 0 && !tracking;
        let lr = self.track_label();
        if let Some(tl) = g.layout(&w(t("help.track")), &g.f.reg, (lr.right - lr.left - self.px(10)) as f32, lh as f32) {
            cv.text(&tl, lr.left + self.px(10), mid_y(&lr), if need { th.accent } else if hot(Hot::Switch) { th.text } else { th.muted });
        }
        // footer: a rule, Clear, Add to search
        let add = self.add_r();
        let rule = add.top - self.px(16);
        cv.fill(&rect(0, rule, self.width(), rule + 1), th.line, 1.0);
        let clr = self.clear_r();
        cv.rrect(&clr, rad(6.0), Some(if hot(Hot::Clear) { th.row } else { th.card }), Some(th.line));
        put_mid(g, cv, t("help.clear"), &g.f.reg, &clr, mid_y(&clr), th.text);
        let ready = d.pick.is_some();
        cv.rrect_a(&add, rad(6.0), Some(th.accent), None, if ready { 1.0 } else { 0.4 });
        if ready && hot(Hot::Add) {
            cv.rrect_a(&add, rad(6.0), Some(ink(th.accent)), None, 0.12);
        }
        put_mid(g, cv, t("help.add"), &g.f.semi, &add, mid_y(&add), ink(th.accent));
    }
}

// the date filter's layout, in CSS px from the top
const DATE_FIELD: i32 = 58;
const DATE_MODE: i32 = 98;
const DATE_CHIPS: i32 = 142;
const DATE_CAL: i32 = 212;
const DATE_GRID: i32 = 268;
const DATE_TOKEN: i32 = DATE_GRID + 6 * 32 + 12;
const DATE_TRACK: i32 = DATE_TOKEN + 34 + 14;
const DATE_FOOT: i32 = DATE_TRACK + 20 + 32;
const DATE_H: i32 = DATE_FOOT + 32 + PAD;

/// The search box's `prefix` filters replaced by `add` (or just gone): searched again as typed.
fn put_filter(prefix: &str, add: Option<&str>) {
    super::with(|u| unsafe {
        let n = GetWindowTextLengthW(u.edit);
        let mut b = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(u.edit, b.as_mut_ptr(), n + 1);
        let q = with_filter(&String::from_utf16_lossy(&b[..got.max(0) as usize]), prefix, add);
        SetWindowTextW(u.edit, wz(&q).as_ptr());
        let end = q.encode_utf16().count();
        SendMessageW(u.edit, 0x00B1, end, end as isize); // EM_SETSEL: the caret after it
    });
}

/// The help for search switch `what` (the date filter for Timestamps), under its button `at` in `owner`'s client.
pub fn open(owner: HWND, what: Hit, at: RECT) {
    close();
    static CLASS: std::sync::Once = std::sync::Once::new();
    let class = wz("MosaicHelp");
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
        let dates = (what == Hit::Timestamps).then(|| {
            let today = Local::now().date_naive();
            let names: Vec<String> = (0..7).map(|i| locale(0x60 + i, false)).collect(); // LOCALE_SSHORTESTDAYNAME1.., Monday on
            Dates {
                field: 0,
                mode: 0,
                month: today.with_day(1).unwrap_or(today),
                pick: None,
                today,
                first: locale(0x100C, true).parse().unwrap_or(0) % 7, // LOCALE_IFIRSTDAYOFWEEK
                days: std::array::from_fn(|i| names[i].clone()),
                title: (NaiveDate::MIN, String::new()),
            }
        });
        // a window first, sized after: the help's height is its text's
        let hwnd = CreateWindowExW(0, class.as_ptr(), wz(title(what)).as_ptr(), WS_POPUP, 0, 0, 1, 1, owner, null_mut(), GetModuleHandleW(null()), null());
        if hwnd.is_null() {
            return;
        }
        let d = Dlg { hwnd, owner, cv: Canvas::new(hwnd, &gfx), gfx, scale, what, dates, hot: None, press: None, tracking: false };
        let (w, h) = (d.width(), if d.dates.is_some() { d.px(DATE_H) } else { d.help_lay().height });
        // under the button, kept on its screen
        let mut p = POINT { x: at.left, y: at.bottom };
        ClientToScreen(owner, &mut p);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST), &mut mi);
        let wa = mi.rcWork;
        let x = p.x.min(wa.right - w - d.px(8)).max(wa.left + d.px(8));
        let mut y = p.y + d.px(6);
        if y + h > wa.bottom - d.px(8) {
            y = (p.y - (at.bottom - at.top) - d.px(6) - h).max(wa.top + d.px(8)); // no room below: above it
        }
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
    }
}

fn close() {
    let Some(p) = DLG.with(|c| c.try_borrow_mut().ok().and_then(|mut c| c.take())) else { return };
    unsafe {
        EnableWindow(p.owner, 1);
        SetForegroundWindow(p.owner);
        DestroyWindow(p.hwnd);
        if std::env::args().any(|a| a.starts_with("--switch-help")) {
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
            let title = with(|p| pt.y < p.px(50) && p.hit(pt.x, pt.y).is_none()).unwrap_or(false);
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
            let up = (wp >> 16) as i16 > 0;
            with(|p| p.turn(if up { -1 } else { 1 }));
            return 0;
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(l);
            with(|p| p.press = p.hit(x, y));
            return 0;
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(l);
            let hit = with(|p| p.press.take().filter(|&b| p.hit(x, y) == Some(b))).flatten();
            if let Some(k) = hit {
                with(|p| p.act(k));
                if hwnd() == Some(h) {
                    InvalidateRect(h, null(), 0);
                }
            }
            return 0;
        }
        WM_KEYDOWN => {
            match wp as u16 {
                VK_ESCAPE => close(),
                VK_RETURN => _ = with(|p| if p.dates.is_some() { p.act(Hot::Add) }),
                k @ (VK_PRIOR | VK_NEXT) => _ = with(|p| p.turn(if k == VK_PRIOR { -1 } else { 1 })),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::search::{EntryView, Matcher, SearchOptions};

    #[test]
    fn the_examples_are_what_search_does() {
        for h in [Hit::MatchCase, Hit::WholeWord, Hit::Regex, Hit::Diacritics] {
            for &(q, name, _, off, on) in examples(h) {
                for (state, want) in [(false, off), (true, on)] {
                    let mut o = SearchOptions { query: q.into(), ..Default::default() };
                    match h {
                        Hit::MatchCase => o.match_case = state,
                        Hit::WholeWord => o.whole_word = state,
                        Hit::Regex => o.regex = state,
                        _ => o.diacritics = state,
                    }
                    let lower = name.to_lowercase();
                    let path = format!("C:\\x\\{name}");
                    let e = EntryView { name, name_lower: &lower, path: &path, path_lower: &path.to_lowercase(), is_dir: false, size: None, modified_ms: None, created_ms: None, accessed_ms: None, attributes: 0 };
                    let got = Matcher::compile(&o).map(|m| m.eval(&e)).unwrap_or(false);
                    assert_eq!(got, want, "{q:?} on {name:?}, switch {state}");
                }
            }
        }
    }

    #[test]
    fn a_date_filter_replaces_its_own_kind_only() {
        assert_eq!(with_filter("cat dm:today", "dm:", Some("dm:2026-10-15")), "cat dm:2026-10-15");
        assert_eq!(with_filter("cat  dc:today ", "dm:", Some("dm:<lastweek")), "cat  dc:today dm:<lastweek");
        assert_eq!(with_filter("DM:2026 cat", "dm:", None), "cat");
        assert_eq!(with_filter("", "dm:", Some("dm:today")), "dm:today");
        let d = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();
        assert_eq!(token(0, 3, Pick::Range(d(2026, 10, 15), Some(d(2026, 10, 1)))), "dm:2026-10-01..2026-10-15");
        assert_eq!(token(1, 1, Pick::Period(2)), "dc:<thisweek");
        assert_eq!(token(2, 2, Pick::Month(d(2026, 3, 1))), "da:>2026-03");
        assert_eq!(period(2, d(2026, 10, 15)), (d(2026, 10, 12), d(2026, 10, 18)));
        assert_eq!(period(5, d(2026, 1, 15)), (d(2025, 12, 1), d(2025, 12, 31)));
    }
}
