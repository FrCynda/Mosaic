//! Files dragged onto the window (from Explorer, another app, or a pane here): onto a tab (its folder), the +
//! (folders open in new tabs), a pane (its folder), a folder in a pane, or a pane's edge (folders open beside it).
//! Copied or moved by the shell (`fileops::transfer`, its progress and questions), never through the clipboard.
//! A tab held over for a moment opens, a folder held over opens in its pane.

use super::*;
use std::sync::atomic::{AtomicU32, Ordering};
use windows_sys::Win32::System::Ole::{RegisterDragDrop, RevokeDragDrop, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE};

pub(super) const WM_DROPPED: u32 = WM_APP + 44; // lParam: a boxed `Dropped`

#[derive(Clone, PartialEq)]
enum Spot {
    Tab(usize),
    NewTab,
    Pane(usize, String),
    Item(usize, String),
    Edge(usize, u8),
}
pub(super) struct Dropped {
    files: Vec<String>,
    spot: Spot,
    effect: u32,
}
/// The drag over the window now: what's dragged, where it points, since when.
struct Over {
    files: Vec<String>,
    spot: Option<Spot>,
    since: Instant,
    sprung: bool,
}
thread_local!(static OVER: RefCell<Option<Over>> = const { RefCell::new(None) });

#[repr(C)]
#[derive(Clone, Copy)]
struct Pointl {
    x: i32,
    y: i32,
}
#[repr(C)]
struct Vtbl {
    qi: unsafe extern "system" fn(*mut Target, *const windows_sys::core::GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut Target) -> u32,
    release: unsafe extern "system" fn(*mut Target) -> u32,
    enter: unsafe extern "system" fn(*mut Target, *mut c_void, u32, Pointl, *mut u32) -> i32,
    over: unsafe extern "system" fn(*mut Target, u32, Pointl, *mut u32) -> i32,
    leave: unsafe extern "system" fn(*mut Target) -> i32,
    drop: unsafe extern "system" fn(*mut Target, *mut c_void, u32, Pointl, *mut u32) -> i32,
}
#[repr(C)]
struct Target {
    vtbl: *const Vtbl,
    refs: AtomicU32,
}
static VTBL: Vtbl = Vtbl { qi, add_ref, release, enter, over, leave, drop: drop_in };

unsafe extern "system" fn qi(this: *mut Target, iid: *const windows_sys::core::GUID, out: *mut *mut c_void) -> i32 {
    let g = &*iid;
    let unknown = g.data1 == 0 && g.data2 == 0 && g.data3 == 0 && g.data4 == [0xC0, 0, 0, 0, 0, 0, 0, 0x46];
    let target = g.data1 == 0x122 && g.data2 == 0 && g.data3 == 0 && g.data4 == [0xC0, 0, 0, 0, 0, 0, 0, 0x46];
    if unknown || target {
        add_ref(this);
        *out = this as *mut c_void;
        0
    } else {
        *out = null_mut();
        0x80004002u32 as i32 // E_NOINTERFACE
    }
}
unsafe extern "system" fn add_ref(this: *mut Target) -> u32 {
    (*this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
unsafe extern "system" fn release(this: *mut Target) -> u32 {
    let n = (*this).refs.fetch_sub(1, Ordering::Relaxed) - 1;
    if n == 0 {
        drop(Box::from_raw(this));
    }
    n
}

/// Drops onto `h` taken (OLE is initialized on this thread).
pub(super) fn register(h: HWND) {
    let t = Box::into_raw(Box::new(Target { vtbl: &VTBL, refs: AtomicU32::new(1) }));
    unsafe {
        if RegisterDragDrop(h, t as *mut c_void) < 0 {
            drop(Box::from_raw(t));
        } else {
            release(t); // the window holds it now
        }
    }
}
pub(super) fn revoke(h: HWND) {
    unsafe { RevokeDragDrop(h) };
}

/// The files a drag carries (CF_HDROP).
fn files(data: *mut c_void) -> Vec<String> {
    use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
    let Some(d) = (unsafe { windows::core::Interface::from_raw_borrowed(&data) }) else { return Vec::new() };
    let d: &IDataObject = d;
    let f = FORMATETC { cfFormat: 15, ptd: null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
    let Ok(mut m) = (unsafe { d.GetData(&f) }) else { return Vec::new() };
    let h = HDROP(unsafe { m.u.hGlobal.0 });
    let n = unsafe { DragQueryFileW(h, u32::MAX, None) };
    let v = (0..n)
        .map(|i| {
            let len = unsafe { DragQueryFileW(h, i, None) } as usize;
            let mut b = vec![0u16; len + 1];
            unsafe { DragQueryFileW(h, i, Some(&mut b)) };
            String::from_utf16_lossy(&b[..len])
        })
        .collect();
    unsafe { windows::Win32::System::Ole::ReleaseStgMedium(&mut m) };
    v
}

fn same(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\').eq_ignore_ascii_case(b.trim_end_matches('\\'))
}
/// What a drop of `files` into `dest` does with keys `keys` held: Ctrl copies, Shift moves, Ctrl+Shift or Alt makes
/// shortcuts; else a move on the same drive, a copy across drives, as Explorer does. Nothing into itself.
fn effect(keys: u32, files: &[String], dest: &str, allowed: u32) -> u32 {
    let (ctrl, shift, alt) = (keys & 8 != 0, keys & 4 != 0, keys & 0x20 != 0);
    if files.iter().any(|f| same(f, dest) || parent_of(f).is_some_and(|p| same(&p, dest)) || dest.to_lowercase().starts_with(&format!("{}\\", f.trim_end_matches('\\').to_lowercase()))) {
        return DROPEFFECT_NONE;
    }
    let want = if ctrl && shift || alt {
        DROPEFFECT_LINK
    } else if ctrl {
        DROPEFFECT_COPY
    } else if shift {
        DROPEFFECT_MOVE
    } else if files.first().and_then(|f| f.get(..2)).is_some_and(|d| dest.get(..2).is_some_and(|e| d.eq_ignore_ascii_case(e))) {
        DROPEFFECT_MOVE
    } else {
        DROPEFFECT_COPY
    };
    if allowed & want != 0 {
        want
    } else if allowed & DROPEFFECT_COPY != 0 {
        DROPEFFECT_COPY
    } else {
        DROPEFFECT_NONE
    }
}
fn parent_of(p: &str) -> Option<String> {
    std::path::Path::new(p).parent().map(|d| d.to_string_lossy().into_owned())
}

impl Ui {
    /// Where a drag at screen point `pt` points, and the box and words to show for it.
    fn spot_at(&mut self, pt: Pointl, files: &[String]) -> Option<(Spot, RECT)> {
        let mut p = POINT { x: pt.x, y: pt.y };
        unsafe { ScreenToClient(self.main, &mut p) };
        let (x, y) = (p.x, p.y);
        let dirs = files.iter().any(|f| is_dir_path(f));
        match self.hits.iter().find(|(r, _)| inside(r, x, y)).map(|h| (h.0, h.1)) {
            Some((_, Hit::Tab(k) | Hit::TabClose(k))) => return Some((Spot::Tab(k), self.drawn(k))),
            Some((r, Hit::NewTab)) if dirs => return Some((Spot::NewTab, r)),
            _ => {}
        }
        let &(id, r) = self.areas.iter().find(|(_, r)| inside(r, x, y))?;
        if dirs {
            let e = self.px(24);
            let d = [x - r.left, r.right - x, y - r.top, r.bottom - y];
            if let Some(side) = (0..4).filter(|&s| d[s] < e).min_by_key(|&s| d[s]) {
                let half = match side {
                    0 => RECT { right: (r.left + r.right) / 2, ..r },
                    1 => RECT { left: (r.left + r.right) / 2, ..r },
                    2 => RECT { bottom: (r.top + r.bottom) / 2, ..r },
                    _ => RECT { top: (r.top + r.bottom) / 2, ..r },
                };
                return Some((Spot::Edge(id, side as u8), half));
            }
        }
        let item = self.with_pane(id, |u| {
            let (lx, ly) = (x - u.org.x, y - u.org.y);
            let i = u.item_at(lx, ly)?;
            let row = u.get(i)?;
            if !row.is_dir {
                return None;
            }
            let (sx, sy) = u.spot(i);
            let wd = if u.grid { u.cell_x(i % u.ncols()).1 - u.cell_x(i % u.ncols()).0 } else { u.cw() };
            let (l, t) = (u.org.x + sx as i32, u.org.y + sy as i32);
            Some((row.path.clone(), RECT { left: l, top: t, right: l + wd, bottom: t + u.rh() }))
        });
        if let Some((d, rr)) = item {
            return Some((Spot::Item(id, d), rr));
        }
        let d = self.pane_dir(id)?;
        Some((Spot::Pane(id, d), r))
    }
    fn spot_dir(&self, s: &Spot) -> Option<String> {
        match s {
            Spot::Tab(k) => listed_dir(&self.focus_q(*k)),
            Spot::Pane(_, d) | Spot::Item(_, d) => Some(d.clone()),
            _ => None,
        }
    }
    /// A drag moved to `pt`: the zone shown, a tab or folder held over opened. Returns the drop's effect.
    fn drag_over(&mut self, keys: u32, pt: Pointl, allowed: u32) -> u32 {
        let Some(mut o) = OVER.with(|o| o.borrow_mut().take()) else { return DROPEFFECT_NONE };
        let got = self.spot_at(pt, &o.files);
        let spot = got.as_ref().map(|g| g.0.clone());
        if spot != o.spot {
            (o.spot, o.since, o.sprung) = (spot.clone(), Instant::now(), false);
        }
        // held over: a tab opens, a folder opens in its pane
        let held = o.since.elapsed().as_secs_f64();
        match &o.spot {
            Some(Spot::Tab(k)) if !o.sprung && held > 0.6 && *k != self.cur => {
                o.sprung = true;
                self.switch_tab(*k);
            }
            Some(Spot::Item(p, d)) if !o.sprung && held > 0.8 => {
                o.sprung = true;
                let (p, d) = (*p, d.clone());
                self.take_to(p, &d);
            }
            _ => {}
        }
        let eff = match &spot {
            None => DROPEFFECT_NONE,
            Some(Spot::NewTab | Spot::Edge(..)) => {
                if allowed & DROPEFFECT_LINK != 0 { DROPEFFECT_LINK } else { DROPEFFECT_COPY & allowed }
            }
            Some(s) => self.spot_dir(s).map_or(DROPEFFECT_NONE, |d| effect(keys, &o.files, &d, allowed)),
        };
        let label = match (&spot, eff) {
            (Some(Spot::NewTab), _) => t("drop.new_tab").to_string(),
            (Some(Spot::Edge(..)), _) => t("drop.beside").to_string(),
            (Some(s), e) if e != DROPEFFECT_NONE => {
                let name = self.spot_dir(s).map_or(String::new(), |d| place_name(&self.browse_q(&d)));
                t(match e {
                    DROPEFFECT_MOVE => "drop.move",
                    DROPEFFECT_LINK => "drop.link",
                    _ => "drop.copy",
                })
                .replace("{name}", &name)
            }
            _ => String::new(),
        };
        self.aim(got.filter(|_| eff != DROPEFFECT_NONE).map(|g| g.1), label);
        self.step_ui();
        OVER.with(|x| *x.borrow_mut() = Some(o));
        eff
    }
}
fn is_dir_path(p: &str) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_dir())
}

unsafe extern "system" fn enter(this: *mut Target, data: *mut c_void, keys: u32, pt: Pointl, eff: *mut u32) -> i32 {
    let fs = files(data);
    if fs.is_empty() {
        *eff = DROPEFFECT_NONE;
        return 0;
    }
    OVER.with(|o| *o.borrow_mut() = Some(Over { files: fs, spot: None, since: Instant::now(), sprung: false }));
    over(this, keys, pt, eff)
}
unsafe extern "system" fn over(_: *mut Target, keys: u32, pt: Pointl, eff: *mut u32) -> i32 {
    let allowed = *eff;
    *eff = with(|u| u.drag_over(keys, pt, allowed)).unwrap_or(DROPEFFECT_NONE);
    if let Some(m) = with(|u| u.main) {
        UpdateWindow(m);
    }
    0
}
unsafe extern "system" fn leave(_: *mut Target) -> i32 {
    OVER.with(|o| o.borrow_mut().take());
    with(|u| {
        u.aim(None, String::new());
        u.step_ui();
    });
    0
}
unsafe extern "system" fn drop_in(this: *mut Target, _: *mut c_void, keys: u32, pt: Pointl, eff: *mut u32) -> i32 {
    let allowed = *eff;
    let e = with(|u| u.drag_over(keys, pt, allowed)).unwrap_or(DROPEFFECT_NONE);
    let o = OVER.with(|o| o.borrow_mut().take());
    leave(this);
    *eff = DROPEFFECT_NONE;
    let Some(Over { files, spot: Some(spot), .. }) = o else { return 0 };
    if e == DROPEFFECT_NONE {
        return 0;
    }
    // done once the drag is over (the source waits for this answer): the shell's progress, its questions
    if let Some(m) = with(|u| u.main) {
        PostMessageW(m, WM_DROPPED, 0, Box::into_raw(Box::new(Dropped { files, spot, effect: e })) as LPARAM);
    }
    // the files are moved here, not by the source: it must not delete them too
    *eff = if e == DROPEFFECT_COPY { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
    0
}

/// WM_DROPPED: what was dropped, done.
pub(super) unsafe fn dropped(l: LPARAM) {
    use crate::infrastructure::fileops;
    let d = *Box::from_raw(l as *mut Dropped);
    let dirs: Vec<String> = d.files.iter().filter(|f| is_dir_path(f)).cloned().collect();
    match d.spot {
        Spot::NewTab => {
            with(|u| {
                for f in &dirs {
                    u.open_tab(f);
                }
            });
        }
        Spot::Edge(p, side) => {
            with(|u| {
                u.activate(p);
                for f in &dirs {
                    u.open_beside(f, side);
                }
            });
        }
        spot => {
            let Some(Some((dest, main))) = with(|u| u.spot_dir(&spot).map(|s| (s, u.main))) else { return };
            let msg = if d.effect == DROPEFFECT_LINK {
                let made: Vec<String> = d
                    .files
                    .iter()
                    .filter_map(|f| {
                        let name = std::path::Path::new(f).file_stem()?.to_string_lossy().into_owned();
                        let lnk = format!("{}\\{name} - Shortcut.lnk", dest.trim_end_matches('\\'));
                        shell::make_link(f, &lnk).then_some(lnk)
                    })
                    .collect();
                let n = made.len();
                if n > 0 {
                    did(Undo::Made(made));
                }
                t("status.copied").replace("{n}", &plural(n))
            } else {
                let copy = d.effect == DROPEFFECT_COPY;
                let to = |p: &String| std::path::Path::new(&dest).join(std::path::Path::new(p).file_name().unwrap_or_default()).to_string_lossy().into_owned();
                let pairs: Vec<(String, String)> = d.files.iter().filter(|p| !std::path::Path::new(&to(p)).exists()).map(|p| (to(p), p.clone())).collect();
                match fileops::transfer(&d.files, &dest, copy, main as isize) {
                    Ok(()) => {
                        if !pairs.is_empty() {
                            did(if copy { Undo::Made(pairs.into_iter().map(|p| p.0).collect()) } else { Undo::Moved(pairs) });
                        }
                        t(if copy { "status.copied" } else { "status.cut" }).replace("{n}", &plural(d.files.len()))
                    }
                    Err(e) if e == "cancelled" => return,
                    Err(e) => t("status.failed").replace("{e}", &e),
                }
            };
            std::thread::sleep(Duration::from_millis(300)); // the USN journal needs a moment to report it
            with(|u| {
                for p in u.visible() {
                    u.with_pane(p, |v| v.run(true));
                }
                u.flash(msg);
                if u.tp.sounds {
                    beep();
                }
            });
        }
    }
}

#[link(name = "user32")]
extern "system" {
    fn MessageBeep(utype: u32) -> i32;
}
/// The system's default sound.
fn beep() {
    unsafe { MessageBeep(0) };
}
