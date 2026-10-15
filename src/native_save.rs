//! Mosaic in place of programs' file windows: Save, Open (upload) and Select folder. `mosaic-dialogs.exe --dialogs`
//! waits for Windows' file dialog to be created in a program the rules allow (Tools > File windows…, see
//! native_apps.rs), makes it invisible at once and opens a Mosaic window
//! (`--save <dialog>`) with a bar along the bottom: name, type, Save/Open/Select folder, Cancel. The answer is typed
//! into the hidden dialog as full paths and its own button pressed, so Firefox sees an ordinary pick. Whatever goes
//! wrong, the real dialog is shown again.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
use windows_sys::Win32::System::Threading::{AttachThreadInput, CreateEventW, GetCurrentThreadId, INFINITE, OpenEventW, OpenProcess, QueryFullProcessImageNameW, SetEvent, EVENT_MODIFY_STATE, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};

use super::*;

/// Set while the watcher runs (it creates it); setting it stops the watcher.
const STOP: &str = "Local\\MosaicSaveDialogsStop";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Mosaic Save window";
/// The name box's control id in the picker (the search box has 0).
pub(super) const ID: usize = 2;
/// The dialog's marks while hidden: see-through, and no taskbar or Alt+Tab entry.
const MARK: i32 = (WS_EX_LAYERED | WS_EX_TOOLWINDOW) as i32;

static ON: AtomicBool = AtomicBool::new(false);
static DIR: Mutex<String> = Mutex::new(String::new());

// ---- another process's dialog -------------------------------------------------------------------------------

/// A message to a window of another process: never waits on one that hangs.
unsafe fn send(h: HWND, m: u32, wp: usize, l: isize) -> isize {
    let mut r = 0usize;
    SendMessageTimeoutW(h, m, wp, l, SMTO_ABORTIFHUNG, 2000, &mut r);
    r as isize
}

unsafe fn text(h: HWND) -> String {
    let mut b = vec![0u16; 1024];
    let n = send(h, WM_GETTEXT, b.len(), b.as_mut_ptr() as isize).clamp(0, 1023) as usize;
    String::from_utf16_lossy(&b[..n])
}

unsafe fn class(h: HWND) -> String {
    let mut b = [0u16; 64];
    let n = GetClassNameW(h, b.as_mut_ptr(), 64);
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

/// The first window under `parent` (at any depth) that `f` accepts.
unsafe fn find(parent: HWND, f: &dyn Fn(HWND) -> bool) -> HWND {
    unsafe extern "system" fn each(c: HWND, l: LPARAM) -> BOOL {
        let s = &mut *(l as *mut (&dyn Fn(HWND) -> bool, HWND));
        if (s.0)(c) {
            s.1 = c;
            return 0;
        }
        1
    }
    let mut s: (&dyn Fn(HWND) -> bool, HWND) = (f, null_mut());
    EnumChildWindows(parent, Some(each), &mut s as *mut _ as LPARAM);
    s.1
}

/// The program file (full path) of the process that owns `h`.
unsafe fn exe_of(h: HWND) -> String {
    let mut pid = 0;
    GetWindowThreadProcessId(h, &mut pid);
    let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if p.is_null() {
        return String::new();
    }
    let mut b = [0u16; 1024];
    let mut n = b.len() as u32;
    let ok = QueryFullProcessImageNameW(p, 0, b.as_mut_ptr(), &mut n) != 0;
    CloseHandle(p);
    if !ok {
        return String::new();
    }
    String::from_utf16_lossy(&b[..n as usize])
}

/// A program's file name, as the rules know it: `firefox.exe`.
pub(super) fn exe_name(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or_default().to_ascii_lowercase()
}

/// What a program calls itself ("Google Chrome"), else its file name without ".exe", capitalised.
pub(super) fn program_name(path: &str) -> String {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    unsafe {
        let p = wz(path);
        let n = GetFileVersionInfoSizeW(p.as_ptr(), null_mut());
        let mut b = vec![0u8; n as usize];
        if n > 0 && GetFileVersionInfoW(p.as_ptr(), 0, n, b.as_mut_ptr() as *mut c_void) != 0 {
            let (mut v, mut len): (*mut c_void, u32) = (null_mut(), 0);
            if VerQueryValueW(b.as_ptr() as *const c_void, wz("\\VarFileInfo\\Translation").as_ptr(), &mut v, &mut len) != 0 && len >= 4 {
                let tr = std::ptr::read_unaligned(v as *const [u16; 2]);
                let key = format!("\\StringFileInfo\\{:04x}{:04x}\\FileDescription", tr[0], tr[1]);
                if VerQueryValueW(b.as_ptr() as *const c_void, wz(&key).as_ptr(), &mut v, &mut len) != 0 && len > 1 {
                    let s = String::from_utf16_lossy(std::slice::from_raw_parts(v as *const u16, len as usize - 1));
                    if !s.trim().is_empty() {
                        return s.trim().to_string();
                    }
                }
            }
        }
    }
    let stem = exe_name(path);
    let mut c = stem.trim_end_matches(".exe").chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// A Save dialog's own controls. Windows puts them in `FloatNotifySink`s: the combo box with an Edit in it is the
/// name, the other the types. An Open dialog has neither (its name box is a `ComboBoxEx32`).
struct Parts {
    name_box: HWND,
    name: HWND,
    types: HWND,
}

/// What a file dialog asks for.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Kind {
    Save,
    Open,
    Folder,
}

/// Told by the name box (measured on Windows 11): a Save dialog's is in a `FloatNotifySink` (see `parts`), an Open
/// dialog's is a `ComboBoxEx32` (1148), a folder picker's a plain Edit (1152). A Save dialog has neither of the last two.
unsafe fn kind_now(dlg: HWND) -> Option<Kind> {
    let shown = |id: i32, cls: &str| !find(dlg, &|c| GetDlgCtrlID(c) == id && class(c) == cls && IsWindowVisible(c) != 0).is_null();
    if parts(dlg).is_some() {
        Some(Kind::Save)
    } else if shown(1148, "ComboBoxEx32") {
        Some(Kind::Open)
    } else if shown(1152, "Edit") {
        Some(Kind::Folder)
    } else {
        None
    }
}

/// The Open or folder dialog's name box (an Edit either way).
unsafe fn name_edit(dlg: HWND) -> HWND {
    let open = find(dlg, &|c| GetDlgCtrlID(c) == 1148 && class(c) == "ComboBoxEx32");
    if open.is_null() {
        find(dlg, &|c| GetDlgCtrlID(c) == 1152 && class(c) == "Edit")
    } else {
        find(open, &|c| class(c) == "Edit")
    }
}

unsafe fn parts(dlg: HWND) -> Option<Parts> {
    let combo = |edit: bool| find(dlg, &|c| class(c) == "ComboBox" && class(GetParent(c)) == "FloatNotifySink" && find(c, &|e| class(e) == "Edit").is_null() != edit);
    let name_box = combo(true);
    if name_box.is_null() {
        return None;
    }
    Some(Parts { name_box, name: find(name_box, &|e| class(e) == "Edit"), types: combo(false) })
}

/// Invisible, off the taskbar. False when Windows refuses (an app run as administrator): it is left as it is.
unsafe fn hide(h: HWND) -> bool {
    SetLastError(0);
    let ex = GetWindowLongW(h, GWL_EXSTYLE);
    if SetWindowLongW(h, GWL_EXSTYLE, ex | MARK) == 0 && GetLastError() != 0 {
        return false;
    }
    SetLayeredWindowAttributes(h, 0, 0, LWA_ALPHA) != 0
}

fn hidden(h: HWND) -> bool {
    unsafe { IsWindow(h) != 0 && GetWindowLongW(h, GWL_EXSTYLE) & WS_EX_TOOLWINDOW as i32 != 0 }
}

/// A dialog that is no file window, back as it was: where it is, its focus its own.
/// ponytail: an unowned one may miss its taskbar button (WS_EX_TOOLWINDOW came off while shown); none seen so far.
unsafe fn reveal(h: HWND) {
    SetWindowLongW(h, GWL_EXSTYLE, GetWindowLongW(h, GWL_EXSTYLE) & !MARK);
    SetWindowPos(h, null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
    RedrawWindow(h, null(), null_mut(), RDW_ERASE | RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN);
}

/// The dialog back on screen, centred over its app's window, in front.
unsafe fn unhide(h: HWND) {
    if !hidden(h) {
        return;
    }
    SetWindowLongW(h, GWL_EXSTYLE, GetWindowLongW(h, GWL_EXSTYLE) & !MARK);
    let (mut d, mut o): (RECT, RECT) = (zeroed(), zeroed());
    GetWindowRect(h, &mut d);
    let owner = GetWindow(h, GW_OWNER);
    if !owner.is_null() && GetWindowRect(owner, &mut o) != 0 {
        // centred over the app's window, kept on its screen (the dialog may be the wider of the two)
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST), &mut mi);
        let a = mi.rcWork;
        let (w, ht) = (d.right - d.left, d.bottom - d.top);
        let x = ((o.left + o.right - w) / 2).min(a.right - w).max(a.left);
        let y = ((o.top + o.bottom - ht) / 2).min(a.bottom - ht).max(a.top);
        SetWindowPos(h, null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
    }
    RedrawWindow(h, null(), null_mut(), RDW_ERASE | RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN);
    front(h);
}

/// `h` to the front. A background process may not take the foreground on its own: it borrows the right from the
/// window that has it, by sharing its input for a moment (measured: plain SetForegroundWindow fails half the time).
unsafe fn front(h: HWND) {
    if SetForegroundWindow(h) != 0 && GetForegroundWindow() == h {
        return;
    }
    let fg = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
    let me = GetCurrentThreadId();
    let joined = fg != me && AttachThreadInput(me, fg, 1) != 0;
    BringWindowToTop(h);
    SetForegroundWindow(h);
    if joined {
        AttachThreadInput(me, fg, 0);
    }
}

// ---- the watcher: mosaic-dialogs.exe --dialogs ----------------------------------------------------------------

/// Runs till `stop()`: no window, no timer; Windows calls it when a window is created.
pub fn watch_main() {
    unsafe {
        let ev = CreateEventW(null(), 0, 0, wz(STOP).as_ptr());
        if ev.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            return; // one is running
        }
        // out of context (nothing loaded into the apps), this process's own windows skipped
        SetWinEventHook(EVENT_OBJECT_CREATE, EVENT_OBJECT_CREATE, null_mut(), Some(created), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        let mut msg: MSG = zeroed();
        loop {
            if MsgWaitForMultipleObjects(1, &ev, 0, INFINITE, QS_ALLINPUT) == WAIT_OBJECT_0 {
                return;
            }
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                DispatchMessageW(&msg);
            }
        }
    }
}

unsafe extern "system" fn created(_: HWINEVENTHOOK, _: u32, h: HWND, obj: i32, child: i32, _: u32, _: u32) {
    // a top-level dialog: Windows' file windows are of this class (as are message boxes and the like)
    if obj != OBJID_WINDOW || child != 0 || h.is_null() || class(h) != "#32770" {
        return;
    }
    let path = exe_of(h);
    let exe = exe_name(&path);
    if exe.is_empty() || exe == "mosaic.exe" {
        return; // Mosaic's own ("Add program…" picks a program in Windows' window)
    }
    // Shift held: the Windows dialog, this once
    let wanted = !super::key_down(VK_SHIFT) && current_rules().wants(&exe);
    if !wanted && programs().iter().any(|p| exe_name(p) == exe) {
        return;
    }
    let h = h as isize;
    // hide() sends the app's window messages and waits for them: on a thread of its own, so an app that hangs
    // stalls only its own dialog, never this hook (it used to, and every file window after it went untaken)
    std::thread::spawn(move || unsafe {
        let h = h as HWND;
        if wanted && hide(h) {
            take(h, &path);
        } else if item(h) {
            add_program(&path); // a program that opens file windows: listed, to be switched on
        }
    });
}

/// Waits for `dlg` to show (its controls made, the app has filled them in by then): whether it is a file window as
/// Windows makes them today (not a message box, nor the old-style file dialog).
unsafe fn item(dlg: HWND) -> bool {
    let t = Instant::now();
    while IsWindow(dlg) != 0 && IsWindowVisible(dlg) == 0 && t.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(5));
    }
    IsWindow(dlg) != 0 && !find(dlg, &|c| class(c) == "DUIViewWndClassName").is_null()
}

/// What a file window asks for (None: not one). Its own controls come a moment after it shows.
unsafe fn kind(dlg: HWND) -> Option<Kind> {
    if !item(dlg) {
        return None;
    }
    let t = Instant::now();
    let mut k = kind_now(dlg);
    while k.is_none() && IsWindow(dlg) != 0 && t.elapsed() < Duration::from_millis(400) {
        std::thread::sleep(Duration::from_millis(10));
        k = kind_now(dlg);
    }
    k.filter(|_| IsWindow(dlg) != 0)
}

/// A dialog just hidden: if it is a file dialog, a picker answers it; anything else is shown again at once.
unsafe fn take(dlg: HWND, path: &str) {
    if !item(dlg) {
        return reveal(dlg);
    }
    add_program(path);
    // the picker starts while the dialog's own controls are still coming, so both are ready at about the same
    // time; if it isn't a file window after all, the picker finds the same as below and quits without a window
    let exe = std::env::current_exe().map(|p| p.with_file_name("mosaic.exe"));
    let picker = exe.and_then(|e| std::process::Command::new(e).args(["--native", "--save", &(dlg as isize).to_string()]).spawn());
    if kind(dlg).is_none() {
        return unhide(dlg);
    }
    let Ok(mut picker) = picker else { return unhide(dlg) };
    SetWindowPos(dlg, null_mut(), -32000, -32000, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE); // out of the way of clicks
    // no window within 3 s: something is wrong, the real dialog it is
    let t = Instant::now();
    loop {
        if picker.try_wait().ok().flatten().is_some() {
            break;
        }
        if t.elapsed() > Duration::from_secs(3) && !super::tabs::windows(null_mut()).iter().any(|w| pid_of(w.0) == picker.id()) {
            let _ = picker.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // the picker is gone (crashed, killed, or done): a dialog it didn't answer comes back
    if IsWindow(dlg) != 0 {
        unhide(dlg);
    }
}

fn pid_of(h: HWND) -> u32 {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(h, &mut pid) };
    pid
}

// ---- turning it on and off (Tools menu) --------------------------------------------------------------------

/// Which programs' file windows Mosaic answers: every one, or (custom) every one but those switched off. In the
/// settings as "dialog_apps"; until changed there, every one.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct Rules {
    pub custom: bool,
    pub apps: std::collections::BTreeMap<String, bool>, // by program file name, lowercase; kept while All is chosen
}

impl Rules {
    pub fn wants(&self, exe: &str) -> bool {
        !self.custom || self.apps.get(exe).copied().unwrap_or(true)
    }
}

fn rules_from(st: &HashMap<String, String>) -> Rules {
    st.get("dialog_apps").and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
}

static RULES: Mutex<Rules> = Mutex::new(Rules { custom: false, apps: std::collections::BTreeMap::new() });

pub(super) fn rules() -> Rules {
    RULES.lock().unwrap().clone()
}

/// Changed in Tools > File windows…: saved at once, the watcher reads it from there.
pub(super) fn set_rules(r: Rules) {
    *RULES.lock().unwrap() = r;
    with(|u| u.save_settings());
}

pub(super) fn rules_json() -> String {
    serde_json::to_string(&*RULES.lock().unwrap()).unwrap_or_default()
}

/// The watcher's copy of the rules: the settings read again only when the file changed.
fn current_rules() -> Rules {
    static SEEN: Mutex<Option<(std::time::SystemTime, Rules)>> = Mutex::new(None);
    let at = std::fs::metadata(settings_path()).and_then(|m| m.modified()).ok();
    let mut seen = SEEN.lock().unwrap();
    match (&*seen, at) {
        (Some((t, r)), Some(at)) if *t == at => r.clone(),
        _ => {
            let r = rules_from(&load_settings());
            *seen = at.map(|at| (at, r.clone()));
            r
        }
    }
}

/// Programs seen opening a file window, or added by hand: full paths, one a line.
fn programs_file() -> std::path::PathBuf {
    settings_path().with_file_name("dialog-programs.txt")
}

pub(super) fn programs() -> Vec<String> {
    std::fs::read_to_string(programs_file()).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()).map(String::from).collect()
}

/// Listed, once (a line added: a few bytes, and only for a program not listed yet).
pub(super) fn add_program(path: &str) {
    use std::io::Write;
    let exe = exe_name(path);
    if exe.is_empty() || programs().iter().any(|p| exe_name(p) == exe) {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(programs_file()) {
        let _ = writeln!(f, "{path}");
    }
}

pub(super) fn remove_program(exe: &str) {
    let rest: Vec<String> = programs().into_iter().filter(|p| exe_name(p) != exe).collect();
    let _ = std::fs::write(programs_file(), rest.iter().map(|p| format!("{p}\n")).collect::<String>());
}

/// Where a program not seen yet lives, if it registered itself (App Paths): firefox.exe before Firefox's first file window.
pub(super) fn app_path(exe: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let key = wz(&format!("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe}"));
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE].into_iter().find_map(|root| unsafe {
        let mut b = [0u16; 1024];
        let mut n = (b.len() * 2) as u32;
        (RegGetValueW(root, key.as_ptr(), null(), RRF_RT_REG_SZ, null_mut(), b.as_mut_ptr() as *mut c_void, &mut n) == 0).then(|| {
            let s = String::from_utf16_lossy(&b[..(n as usize / 2).saturating_sub(1)]);
            s.trim_matches('"').to_string()
        })
    })
}

pub(super) fn load(st: &HashMap<String, String>) {
    *RULES.lock().unwrap() = rules_from(st);
    ON.store(st.get("save_dialog").is_none_or(|v| v == "1"), Ordering::Relaxed); // on until turned off
    *DIR.lock().unwrap() = st.get("save_dir").cloned().unwrap_or_default();
    if ON.load(Ordering::Relaxed) && !active() {
        register(); // the first time
        start(); // after an update, or if it was ended
    }
}

/// The folder last saved to, for when the dialog doesn't say.
pub(super) fn last_dir() -> String {
    DIR.lock().unwrap().clone()
}

pub(super) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

pub(super) fn set_on(v: bool) {
    ON.store(v, Ordering::Relaxed);
    let key = wz(RUN_KEY);
    let name = wz(RUN_VALUE);
    unsafe {
        if v {
            register();
            start();
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr());
            let ev = OpenEventW(EVENT_MODIFY_STATE, 0, wz(STOP).as_ptr());
            if !ev.is_null() {
                SetEvent(ev);
                CloseHandle(ev);
            }
        }
    }
}

/// Started with Windows: written only when it is not there yet (no write each launch).
fn register() {
    use windows_sys::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_SZ};
    let (key, name) = (wz(RUN_KEY), wz(RUN_VALUE));
    let Ok(exe) = super::dialogs_exe() else { return };
    let cmd = format!("\"{}\" --dialogs", exe.display());
    unsafe {
        let mut b = [0u16; 1024];
        let mut n = (b.len() * 2) as u32;
        if RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), b.as_mut_ptr() as *mut c_void, &mut n) == 0
            && String::from_utf16_lossy(&b[..(n as usize / 2).saturating_sub(1)]) == cmd
        {
            return;
        }
        let cmd = wz(&cmd);
        RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, cmd.as_ptr() as *const c_void, (cmd.len() * 2) as u32);
    }
}

fn start() {
    if unsafe { running() } {
        return;
    }
    if let Ok(exe) = super::dialogs_exe() {
        let _ = std::process::Command::new(exe).arg("--dialogs").spawn();
    }
}

unsafe fn running() -> bool {
    let ev = OpenEventW(EVENT_MODIFY_STATE, 0, wz(STOP).as_ptr());
    !ev.is_null() && CloseHandle(ev) != 0
}

// ---- the picker: mosaic.exe --save <dialog> ------------------------------------------------------------------

struct Pick {
    dlg: HWND,
    kind: Kind,
    main: HWND,
    edit: HWND, // the name box
    types: Vec<String>,
    ty: usize,
    name: String, // as the app suggested it
    app: String,
    start: String,
    /// What the list's selection put in the name box, and the full paths it stands for (files found by a search
    /// are in many folders): used as long as the box still says that.
    chosen: (String, Vec<String>),
    done: bool, // answered: closing the window no longer cancels
}

thread_local!(static PICK: RefCell<Option<Pick>> = const { RefCell::new(None) });

fn pick<R>(f: impl FnOnce(&mut Pick) -> R) -> Option<R> {
    PICK.with(|p| p.borrow_mut().as_mut().map(f))
}

/// A Save/Open/folder window, this one.
pub(super) fn active() -> bool {
    PICK.with(|p| p.borrow().is_some())
}

pub(super) fn kind_of() -> Option<Kind> {
    pick(|p| p.kind)
}

/// `--save <dialog>`: read what the dialog suggests. Started for a dialog that is gone already: nothing to do.
pub(super) fn begin() {
    let args: Vec<String> = std::env::args().collect();
    let Some(dlg) = args.iter().position(|a| a == "--save").and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<isize>().ok()) else { return };
    let dlg = dlg as HWND;
    unsafe {
        let Some(kind) = kind(dlg) else { std::process::exit(0) };
        let (mut types, mut ty, mut name) = (Vec::new(), 0, String::new());
        if kind == Kind::Save {
            let Some(p) = parts(dlg) else { std::process::exit(0) };
            let n = send(p.types, CB_GETCOUNT, 0, 0).max(0) as usize;
            types = (0..n)
                .map(|i| {
                    let len = send(p.types, CB_GETLBTEXTLEN, i, 0).clamp(0, 1000) as usize;
                    let mut b = vec![0u16; len + 1];
                    send(p.types, CB_GETLBTEXT, i, b.as_mut_ptr() as isize);
                    String::from_utf16_lossy(&b[..len])
                })
                .collect();
            ty = (send(p.types, CB_GETCURSEL, 0, 0).max(0) as usize).min(n.saturating_sub(1));
            name = text(p.name);
        }
        let app = program_name(&exe_of(dlg));
        let start = dialog_dir(dlg).or_else(|| Some(DIR.lock().unwrap().clone()).filter(|d| std::path::Path::new(d).is_dir())).or_else(|| side::user_dirs().into_iter().next()).unwrap_or_else(|| "C:\\".into());
        PICK.set(Some(Pick { dlg, kind, main: null_mut(), edit: null_mut(), types, ty, name, app, start, chosen: Default::default(), done: false }));
    }
}

/// The folder the dialog opened in. Its address bar shows a path, or for the user's own folders just their name.
unsafe fn dialog_dir(dlg: HWND) -> Option<String> {
    let bar = find(dlg, &|c| GetDlgCtrlID(c) == 1001 && class(c) == "ToolbarWindow32");
    let s = text(bar);
    let shown = s.split_once(": ").map_or(s.as_str(), |x| x.1).trim();
    if shown.contains(":\\") && std::path::Path::new(shown).is_dir() {
        return Some(shown.to_string());
    }
    side::user_dirs().into_iter().find(|d| {
        let mut fi: SHFILEINFOW = zeroed();
        SHGetFileInfoW(wz(d).as_ptr(), 0, &mut fi, size_of::<SHFILEINFOW>() as u32, 0x200); // SHGFI_DISPLAYNAME
        let n = fi.szDisplayName.iter().position(|&c| c == 0).unwrap_or(0);
        !shown.is_empty() && String::from_utf16_lossy(&fi.szDisplayName[..n]).eq_ignore_ascii_case(shown)
    })
}

pub(super) fn start_dir() -> Option<String> {
    PICK.with(|p| p.borrow().as_ref().map(|p| p.start.clone()))
}

/// The button that answers, which also names the window: Save, Open, Select folder.
fn verb(k: Kind) -> &'static str {
    match k {
        Kind::Save => t("save.save"),
        Kind::Open => t("save.open"),
        Kind::Folder => t("save.select"),
    }
}

/// "Save as — Firefox", "Open — Firefox", "Select folder — Firefox".
pub(super) fn title() -> Option<String> {
    pick(|p| format!("{} — {}", if p.kind == Kind::Save { t("save.title") } else { verb(p.kind) }, p.app))
}

/// The name box, beside the window's other controls. Firefox clicked (which would only bring its invisible
/// dialog forward) brings this window forward instead.
pub(super) unsafe fn create(main: HWND, font: HFONT) {
    let Some((name, dlg)) = pick(|p| (p.name.clone(), p.dlg)) else { return };
    let e = CreateWindowExW(0, wz("EDIT").as_ptr(), null(), WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32, 0, 0, 10, 10, main, ID as HMENU, GetModuleHandleW(null()), null());
    SetWindowTheme(e, wz("DarkMode_CFD").as_ptr(), null());
    SendMessageW(e, WM_SETFONT, font as usize, 0);
    SetWindowTextW(e, wz(&name).as_ptr());
    pick(|p| (p.main, p.edit) = (main, e));
    SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, null_mut(), Some(foreground), pid_of(dlg), 0, WINEVENT_OUTOFCONTEXT);
}

unsafe extern "system" fn foreground(_: HWINEVENTHOOK, _: u32, h: HWND, _: i32, _: i32, _: u32, _: u32) {
    if let Some(main) = pick(|p| (p.dlg == h && !p.done).then_some(p.main)).flatten() {
        front(main);
    }
}

/// The first time (after that, where it was last left): centred at 80 % on the screen of the app's window.
pub(super) unsafe fn show(main: HWND) {
    if let Some(dlg) = pick(|p| p.dlg) {
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(MonitorFromWindow(GetWindow(dlg, GW_OWNER), MONITOR_DEFAULTTONEAREST), &mut mi) != 0 {
            let a = mi.rcWork;
            let (w, h) = ((a.right - a.left) * 4 / 5, (a.bottom - a.top) * 4 / 5);
            SetWindowPos(main, null_mut(), a.left + (a.right - a.left - w) / 2, a.top + (a.bottom - a.top - h) / 2, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
    ShowWindow(main, SW_SHOW);
}

/// On screen (its first frame drawn): in front. Windows refuses that to a window still cloaked.
pub(super) unsafe fn on_screen() {
    if let Some(main) = pick(|p| p.main) {
        front(main);
    }
}

/// Shown: the name's stem selected for typing over, as Explorer does.
pub(super) unsafe fn shown() {
    let Some((e, name)) = pick(|p| (p.edit, p.name.clone())) else { return };
    SetFocus(e);
    let stem = name.rfind('.').filter(|&k| k > 0).unwrap_or(name.len());
    SendMessageW(e, 0x00B1, 0, name[..stem].encode_utf16().count() as isize);
}

pub(super) fn focus() {
    pick(|p| unsafe { SetFocus(p.edit) });
}

pub(super) fn is_name(h: HWND) -> bool {
    pick(|p| p.edit == h).unwrap_or(false)
}

/// Names as an Open dialog takes several: "a.txt" "b.txt".
fn quoted(names: &[String]) -> String {
    if names.len() == 1 {
        return names[0].clone();
    }
    names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(" ")
}

/// What was typed, as names: `"a" "b"` is two, anything else one.
fn unquote(s: &str) -> Vec<String> {
    let s = s.trim();
    if !s.starts_with('"') {
        return if s.is_empty() { Vec::new() } else { vec![s.to_string()] };
    }
    s.split('"').skip(1).step_by(2).map(str::trim).filter(|n| !n.is_empty()).map(String::from).collect()
}

fn file_name(p: &str) -> String {
    std::path::Path::new(p).file_name().map_or_else(|| p.to_string(), |n| n.to_string_lossy().into_owned())
}

/// The list's selection in the name box, unless something was typed there. Save: one file's name. Open: the files.
/// Folder: the one folder picked, else the folder on screen.
pub(super) fn picked(u: &Ui) {
    let Some(kind) = kind_of() else { return };
    let rows: Vec<(String, bool)> = u.sel.iter().filter_map(|i| u.get(i)).map(|r| (r.path.clone(), r.is_dir)).collect();
    let paths: Vec<String> = match kind {
        Kind::Save if rows.len() == 1 && !rows[0].1 => vec![rows[0].0.clone()],
        Kind::Open => rows.iter().filter(|r| !r.1).map(|r| r.0.clone()).collect(),
        Kind::Folder if rows.len() == 1 && rows[0].1 => vec![rows[0].0.clone()],
        Kind::Folder if rows.is_empty() => listed_dir(&u.args.q).into_iter().collect(),
        _ => Vec::new(),
    };
    if paths.is_empty() {
        return;
    }
    let shown = quoted(&paths.iter().map(|p| file_name(p)).collect::<Vec<_>>());
    pick(|p| unsafe {
        // typed in: kept; still what the list put there (or empty): follows it
        if GetFocus() != p.edit || get_text(p.edit) == p.chosen.0 {
            SetWindowTextW(p.edit, wz(&shown).as_ptr());
            p.chosen = (shown, paths);
        }
    });
}

/// The dialog went away by itself (the app closed it): so does this window.
pub(super) fn poll() {
    pick(|p| unsafe {
        if !p.done && IsWindow(p.dlg) == 0 {
            p.done = true;
            PostMessageW(p.main, WM_CLOSE, 0, 0);
        }
    });
}

/// The window closing without an answer (×, Alt+F4) is Cancel.
pub(super) fn closing() {
    pick(|p| unsafe {
        if !p.done {
            p.done = true;
            PostMessageW(p.dlg, WM_COMMAND, IDCANCEL as usize, 0);
        }
    });
}

pub(super) unsafe fn cancel(main: HWND) {
    closing();
    PostMessageW(main, WM_CLOSE, 0, 0);
}

/// "Windows dialog": the real one, in place of this window.
pub(super) unsafe fn give_back(main: HWND) {
    if let Some(dlg) = pick(|p| {
        p.done = true;
        p.dlg
    }) {
        ShowWindow(main, SW_HIDE);
        unhide(dlg);
    }
    PostMessageW(main, WM_CLOSE, 0, 0);
}

/// The extensions a type's label lists: "Web Page, complete (*.htm;*.html)" → htm, html. None for "*.*".
fn exts(label: &str) -> Vec<String> {
    let Some(inner) = label.rfind('(').map(|k| &label[k + 1..]).and_then(|s| s.split(')').next()) else { return Vec::new() };
    inner.split(';').filter_map(|p| p.trim().strip_prefix("*.")).filter(|e| !e.is_empty() && !e.contains('*')).map(str::to_lowercase).collect()
}

fn ext(name: &str) -> String {
    name.rsplit_once('.').map(|x| x.1.to_lowercase()).unwrap_or_default()
}

/// `name` with type `to`'s extension: in place of one from `from`'s list, else added. Kept when `to` takes any.
fn with_ext(name: &str, from: &[String], to: &[String]) -> String {
    let Some(new) = to.first() else { return name.to_string() };
    if to.contains(&ext(name)) {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, e)) if from.contains(&e.to_lowercase()) => format!("{stem}.{new}"),
        _ => format!("{name}.{new}"),
    }
}

/// Names Windows keeps for devices, whatever the extension.
fn reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit() && stem.as_bytes()[3] != b'0')
}

unsafe fn get_text(h: HWND) -> String {
    let n = GetWindowTextLengthW(h);
    let mut b = vec![0u16; n as usize + 1];
    GetWindowTextW(h, b.as_mut_ptr(), n + 1);
    String::from_utf16_lossy(&b[..n as usize])
}

/// Something in the way: with Windows' own warning sound (the user's sound scheme, nothing of ours).
unsafe fn say(main: HWND, s: &str) {
    MessageBoxW(main, wz(s).as_ptr(), wz("Mosaic").as_ptr(), MB_OK | MB_ICONWARNING);
}

/// The type dropdown: another type puts its extension on the name.
pub(super) unsafe fn type_menu(main: HWND) {
    let Some(Some((r, gap))) = with(|u| u.hits.iter().find(|x| x.1 == Hit::SaveType).map(|x| (x.0, u.px(4)))) else { return };
    let Some((types, cur)) = pick(|p| (p.types.clone(), p.ty)) else { return };
    let mut p = POINT { x: r.left, y: r.bottom + gap };
    let mut top = POINT { x: r.left, y: r.top - gap };
    ClientToScreen(main, &mut p);
    ClientToScreen(main, &mut top);
    let items = types.iter().enumerate().map(|(i, s)| (s.clone(), String::new(), i == cur)).collect();
    let Some(k) = popup(main, p.x, p.y, top.y, r.right - r.left, true, items, false) else { return };
    pick(|p| {
        let name = get_text(p.edit);
        if !name.trim().is_empty() {
            SetWindowTextW(p.edit, wz(&with_ext(&name, &exts(&p.types[p.ty]), &exts(&p.types[k]))).as_ptr());
        }
        p.ty = k;
    });
    InvalidateRect(main, null(), 0);
}

/// A file or folder opened in the list. Save: a folder is listed, a file is the one to save over. Open: the file
/// (with the rest of the selection, if it is part of it) is the answer. Folder: a folder is listed.
pub(super) unsafe fn open(main: HWND, path: &str) {
    let Some(kind) = kind_of() else { return };
    let dir = std::fs::metadata(path).is_ok_and(|m| m.is_dir()) || crate::direct::server(path);
    match kind {
        _ if dir => {
            with(|u| u.go_dir(path));
        }
        Kind::Save => {
            pick(|p| SetWindowTextW(p.edit, wz(&file_name(path)).as_ptr()));
            commit(main);
        }
        Kind::Open => {
            // the box shows the selection when the opened file is in it; else just this one
            pick(|p| {
                if !p.chosen.1.iter().any(|c| c == path) {
                    p.chosen = (file_name(path), vec![path.to_string()]);
                    SetWindowTextW(p.edit, wz(&p.chosen.0).as_ptr());
                }
            });
            commit(main);
        }
        Kind::Folder => {}
    }
}

/// Save / Open / Select folder: checked here (so no Windows box pops up behind the hidden dialog), then handed to
/// the dialog.
pub(super) unsafe fn commit(main: HWND) {
    let Some((dlg, e, kind, done)) = pick(|p| (p.dlg, p.edit, p.kind, p.done)) else { return };
    if done {
        return; // answered already (a few files opened at once)
    }
    let typed = get_text(e);
    let dir = with(|u| listed_dir(&u.args.q)).flatten().filter(|d| std::path::Path::new(d).is_dir());
    // what the selection put there, else what was typed: full paths, or names in the folder on screen
    let chosen = pick(|p| (p.chosen.0 == typed).then(|| p.chosen.1.clone())).flatten();
    let at = |n: &str| match &dir {
        _ if std::path::Path::new(n).is_absolute() => Some(n.to_string()),
        Some(d) => Some(std::path::Path::new(d).join(n).to_string_lossy().into_owned()),
        None => None,
    };
    match kind {
        Kind::Save => commit_save(main, dlg, e, &typed, dir),
        Kind::Open => {
            let paths = match chosen {
                Some(c) => c,
                None => {
                    let names = unquote(&typed);
                    if names.is_empty() {
                        SetFocus(e);
                        return;
                    }
                    let Some(paths) = names.iter().map(|n| at(n)).collect::<Option<Vec<_>>>() else { return say(main, t("save.no_folder")) };
                    paths
                }
            };
            // a folder typed in: listed here
            if paths.len() == 1 && std::path::Path::new(&paths[0]).is_dir() {
                SetWindowTextW(e, wz("").as_ptr());
                with(|u| u.go_dir(&paths[0]));
                return;
            }
            if let Some(gone) = paths.iter().find(|p| !std::path::Path::new(p).is_file()) {
                return say(main, &t("save.not_found").replace("{name}", &file_name(gone)));
            }
            hand_over(main, dlg, &quoted(&paths), false, paths.first().and_then(|p| std::path::Path::new(p).parent()).map(|d| d.to_string_lossy().into_owned()));
        }
        Kind::Folder => {
            let target = match chosen {
                Some(c) => c.into_iter().next(),
                None if typed.trim().is_empty() => dir.clone(),
                None => at(typed.trim()),
            };
            let Some(target) = target.filter(|t| std::path::Path::new(t).is_dir()) else {
                return say(main, &t("save.not_found").replace("{name}", typed.trim()));
            };
            hand_over(main, dlg, &target, true, Some(target.clone()));
        }
    }
}

/// The answer typed into the hidden dialog's name box, then its button. A folder dialog first goes into the folder
/// typed (that is what its button does with a path), then takes the folder it is in. If the dialog stays (it said
/// no, with a message of its own), the real dialog it is.
unsafe fn hand_over(main: HWND, dlg: HWND, answer: &str, folder: bool, remember: Option<String>) {
    pick(|p| p.done = true);
    ShowWindow(main, SW_HIDE);
    let e = name_edit(dlg);
    if !e.is_null() {
        send(e, WM_SETTEXT, 0, wz(answer).as_ptr() as isize);
        PostMessageW(dlg, WM_COMMAND, IDOK as usize, 0);
        if folder {
            // in it once its box shows the folder's name in place of the path (measured: ~50 ms)
            let t0 = Instant::now();
            while IsWindow(dlg) != 0 && text(e) == answer && t0.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(20));
            }
            send(e, WM_SETTEXT, 0, wz("").as_ptr() as isize);
            PostMessageW(dlg, WM_COMMAND, IDOK as usize, 0);
        }
    }
    finish(main, dlg, remember);
}

/// Waits for the dialog to take the answer: remembered when it did, the real dialog shown when it didn't.
unsafe fn finish(main: HWND, dlg: HWND, remember: Option<String>) -> bool {
    let t0 = Instant::now();
    while IsWindow(dlg) != 0 && t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let taken = IsWindow(dlg) == 0;
    if !taken {
        unhide(dlg);
    } else if let Some(d) = remember {
        *DIR.lock().unwrap() = d;
        with(|u| u.save_settings());
    }
    PostMessageW(main, WM_CLOSE, 0, 0);
    taken
}

unsafe fn commit_save(main: HWND, dlg: HWND, e: HWND, typed: &str, dir: Option<String>) {
    let Some((types, ty)) = pick(|p| (p.types.clone(), p.ty)) else { return };
    let mut name = typed.trim().trim_end_matches(['.', ' ']).to_string();
    if name.is_empty() || name.contains(|c: char| BAD_NAME.contains(c) || c < ' ') {
        SetFocus(e);
        if !name.is_empty() {
            bad_name_tip(e);
        }
        return;
    }
    if reserved(&name) {
        return say(main, &t("save.reserved").replace("{name}", &name));
    }
    let want = types.get(ty).map(|s| exts(s)).unwrap_or_default();
    if !want.is_empty() && !want.contains(&ext(&name)) {
        name = format!("{name}.{}", want[0]);
    }
    let Some(dir) = dir else {
        return say(main, t("save.no_folder"));
    };
    let path = std::path::Path::new(&dir).join(&name).to_string_lossy().into_owned();
    match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => return say(main, &t("save.is_folder").replace("{name}", &name)),
        Ok(_) => {
            let ask = wz(&t("save.replace").replace("{name}", &name));
            if MessageBoxW(main, ask.as_ptr(), wz("Mosaic").as_ptr(), MB_OKCANCEL | MB_ICONWARNING) != IDOK {
                return;
            }
            // the old one to the Recycle Bin first: the dialog then has nothing to ask
            if crate::infrastructure::fileops::delete_many(&[path.clone()], true).is_err() {
                return say(main, t("save.cant_replace"));
            }
        }
        Err(_) => {}
    }
    pick(|p| p.done = true);
    ShowWindow(main, SW_HIDE);
    let Some(p) = parts(dlg) else {
        unhide(dlg);
        PostMessageW(main, WM_CLOSE, 0, 0);
        return;
    };
    // the type first (the dialog rewrites the name's extension when it changes), then the name, then its Save
    if !p.types.is_null() {
        let id = GetDlgCtrlID(p.types) as usize;
        send(p.types, CB_SETCURSEL, ty, 0);
        send(GetParent(p.types), WM_COMMAND, (CBN_SELENDOK as usize) << 16 | id, p.types as isize);
        send(GetParent(p.types), WM_COMMAND, (CBN_SELCHANGE as usize) << 16 | id, p.types as isize);
    }
    send(p.name, WM_SETTEXT, 0, wz(&path).as_ptr() as isize);
    send(GetParent(p.name_box), WM_COMMAND, (CBN_EDITCHANGE as usize) << 16 | GetDlgCtrlID(p.name_box) as usize, p.name_box as isize);
    PostMessageW(dlg, WM_COMMAND, IDOK as usize, 0);
    if finish(main, dlg, Some(dir)) {
        crate::recent::note(&path, false);
    }
}

// ---- the bar along the bottom ------------------------------------------------------------------------------

/// "File name:", or "Folder:" when picking one.
fn label(k: Kind) -> &'static str {
    if k == Kind::Folder { t("save.folder_label") } else { t("save.name_label") }
}

/// As Windows' own: a row per box, full width, labels right-aligned before them; a gap; [Windows dialog] at the
/// left and [Save] [Cancel] at the right. Below the status bar, at the very bottom of the window.
impl Ui {
    /// The bar's height (0: not a Save/Open/folder window). Two rows for a Save with types, else one.
    pub(super) fn save_height(&self, box_h: i32) -> i32 {
        let Some(rows) = pick(|p| 1 + (p.kind == Kind::Save && !p.types.is_empty()) as i32) else { return 0 };
        self.px(14) + rows * box_h + (rows - 1) * self.px(6) + self.px(30) + box_h + self.px(14)
    }

    pub(super) fn place_save(&mut self, wd: i32, box_h: i32, text_h: i32, hits: &mut Vec<(RECT, Hit)>) {
        let Some((e, types, kind)) = pick(|p| (p.edit, p.types.clone(), p.kind)) else { return };
        let (g, px) = (&self.gfx, |v: i32| (v as f64 * self.scale).round() as i32);
        let ht = self.save_top + self.save_height(box_h);
        let col = px(16) + g.width(label(kind), &g.f.reg).max(if types.is_empty() { 0 } else { g.width(t("save.type_label"), &g.f.reg) }) + px(8);
        let row = |top: i32, l: i32, r: i32| RECT { left: l, top, right: r, bottom: top + box_h };
        let name = row(self.save_top + px(14), col, wd - px(16));
        hits.push((name, Hit::SaveName));
        unsafe { MoveWindow(e, name.left + px(8), name.top + (box_h - text_h) / 2, (name.right - name.left - px(16)).max(1), text_h, 1) };
        if !types.is_empty() {
            hits.push((row(name.bottom + px(6), col, wd - px(16)), Hit::SaveType));
        }
        let top = ht - px(14) - box_h;
        let btn = |s: &str| (g.width(s, &g.f.reg) + px(32)).max(px(96));
        let cancel = row(top, wd - px(16) - btn(t("save.cancel")), wd - px(16));
        let ok = row(top, cancel.left - px(10) - btn(verb(kind)), cancel.left - px(10));
        hits.push((cancel, Hit::SaveCancel));
        hits.push((ok, Hit::SaveOk));
        hits.push((row(top, px(10), px(10) + g.width(t("save.windows"), &g.f.reg) + px(36)), Hit::SaveWin));
    }

    pub(super) fn paint_save(&self, wd: i32) {
        let Some((e, types, ty, kind)) = pick(|p| (p.edit, p.types.clone(), p.ty, p.kind)) else { return };
        let Some(&(name_r, _)) = self.hits.iter().find(|x| x.1 == Hit::SaveName) else { return };
        let (g, cv, lh) = (&self.gfx, &self.cm, self.gfx.f.lh);
        let px = |v: i32| (v as f64 * self.scale).round() as i32;
        let rad = |v: f64| (v * self.scale) as f32;
        let all = RECT { left: 0, top: self.save_top, right: wd, bottom: self.save_top + 100_000 };
        cv.fill(&all, mix(theme().card, theme().text, 0.04), 1.0); // a shade off the bars: lighter on dark, darker on light
        cv.fill(&RECT { bottom: self.save_top + 1, ..all }, theme().line, 1.0);
        let ty_of = |r: &RECT| r.top + (r.bottom - r.top - lh) / 2;
        // labels end where the boxes begin
        let right_label = |s: &str, r: &RECT| put(g, cv, s, &g.f.reg, r.left - px(8) - g.width(s, &g.f.reg), ty_of(r), theme().text);
        right_label(label(kind), &name_r);
        let focused = unsafe { GetFocus() } == e;
        cv.rrect(&name_r, rad(4.0), Some(theme().bg), Some(if focused { theme().accent } else { theme().line }));
        let hot = |h: Hit| self.hot == Some(h);
        for &(r, h) in &self.hits {
            match h {
                Hit::SaveType => {
                    right_label(t("save.type_label"), &r);
                    cv.rrect(&r, rad(4.0), Some(theme().bg), Some(if hot(h) { theme().muted } else { theme().line }));
                    if let Some(l) = g.layout(&w(types.get(ty).map_or("", |s| s.as_str())), &g.f.reg, (r.right - r.left - px(34)) as f32, lh as f32) {
                        cv.text(&l, r.left + px(9), ty_of(&r), theme().text);
                    }
                    cv.chevron((r.right - px(13)) as f32, (r.top + r.bottom) as f32 / 2.0, rad(3.5), true, theme().text, rad(1.25));
                }
                Hit::SaveOk => {
                    // the default button: filled with the accent
                    cv.rrect(&r, rad(4.0), Some(if hot(h) { mix(theme().accent, theme().text, 0.15) } else { theme().accent }), None);
                    put_mid(g, cv, verb(kind), &g.f.reg, &r, ty_of(&r), theme().bg);
                }
                Hit::SaveCancel => {
                    cv.rrect(&r, rad(4.0), Some(if hot(h) { theme().hl } else { theme().card }), Some(theme().line));
                    put_mid(g, cv, t("save.cancel"), &g.f.reg, &r, ty_of(&r), theme().text);
                }
                Hit::SaveWin => {
                    // where Windows has "Hide Folders": a quiet link, ˄ before it
                    if hot(h) {
                        cv.rrect(&r, rad(4.0), Some(theme().hl), None);
                    }
                    let c = if hot(h) { theme().text } else { theme().muted };
                    cv.chevron((r.left + px(13)) as f32, (r.top + r.bottom) as f32 / 2.0, rad(3.5), false, c, rad(1.25));
                    put(g, cv, t("save.windows"), &g.f.reg, r.left + px(26), ty_of(&r), c);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_give_their_extensions() {
        assert_eq!(exts("Web Page, complete (*.htm;*.html)"), ["htm", "html"]);
        assert_eq!(exts("Text Files (*.txt;*.text)"), ["txt", "text"]);
        assert!(exts("All Files (*.*)").is_empty());
        assert!(exts("Web Page, HTML only").is_empty());
    }

    #[test]
    fn another_type_swaps_the_extension() {
        let (page, text) = (exts("Web (*.htm;*.html)"), exts("Text (*.txt)"));
        assert_eq!(with_ext("a.html", &page, &text), "a.txt");
        assert_eq!(with_ext("a.b.htm", &page, &text), "a.b.txt");
        assert_eq!(with_ext("v1.2", &page, &text), "v1.2.txt"); // not a type's extension: kept, one added
        assert_eq!(with_ext("a.txt", &page, &text), "a.txt");
        assert_eq!(with_ext("a.html", &page, &[]), "a.html"); // All files: as typed
    }

    #[test]
    fn several_names_go_in_quotes() {
        let two = ["a b.txt".to_string(), "c.txt".to_string()];
        assert_eq!(quoted(&two), "\"a b.txt\" \"c.txt\"");
        assert_eq!(unquote(&quoted(&two)), two);
        assert_eq!(quoted(&two[..1]), "a b.txt");
        assert_eq!(unquote("  a b.txt "), ["a b.txt"]);
        assert!(unquote("  ").is_empty());
    }

    #[test]
    fn rules_pick_programs() {
        assert_eq!(exe_name("C:\\Program Files\\Mozilla Firefox\\Firefox.EXE"), "firefox.exe");
        // until changed: every program (an old Firefox-only setting too)
        assert!(rules_from(&HashMap::new()).wants("chrome.exe"));
        let st = |v: &str| HashMap::from([("dialog_apps".to_string(), v.to_string())]);
        assert!(rules_from(&st(r#"{"all":false,"apps":{"firefox.exe":true}}"#)).wants("chrome.exe"));
        // custom: every program but those switched off; All ignores the switches
        let r = rules_from(&st(r#"{"custom":true,"apps":{"notepad.exe":false}}"#));
        assert!(r.wants("chrome.exe") && !r.wants("notepad.exe"));
        assert!(rules_from(&st(r#"{"custom":false,"apps":{"notepad.exe":false}}"#)).wants("notepad.exe"));
    }

    #[test]
    fn device_names_are_refused() {
        for n in ["CON", "nul.txt", "com1.html", "LPT9"] {
            assert!(reserved(n), "{n}");
        }
        for n in ["console", "com0", "comx", "a.con", "nullable"] {
            assert!(!reserved(n), "{n}");
        }
    }
}
