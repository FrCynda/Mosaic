//! The preview pane at the right of the list (Alt+P): the one selected file, shown or played.
//! - video and audio play with Media Foundation (MFPlay), under our own play / seek / time bar
//! - documents (PDF, Office, …) use the preview handler Explorer's preview pane would, which Windows runs in its
//!   own process (prevhost), so a misbehaving handler can't take the app down and its memory isn't ours
//! - text and code are drawn here
//! - pictures, and anything else, come from the shell's thumbnail helper at the pane's size (else the file's icon)

use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows::core::{Interface, PCWSTR, PROPVARIANT, PWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, HWND as WinHwnd, RECT as WinRect};
use windows::Win32::Media::MediaFoundation::{
    IMFPMediaPlayer, IMFPMediaPlayerCallback, MFPCreateMediaPlayer, MFStartup, MFP_CREATION_OPTIONS, MFP_MEDIAPLAYER_STATE_PLAYING,
    MFP_MEDIAPLAYER_STATE_STOPPED, MFP_POSITIONTYPE_100NS,
};
use windows::Win32::System::Com::{CLSIDFromString, CoCreateInstance, IStream, CLSCTX_LOCAL_SERVER};
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithFile, IInitializeWithStream};
use windows::Win32::UI::Shell::{
    AssocQueryStringW, IInitializeWithItem, IPreviewHandler, IPreviewHandlerVisuals, IShellItem, SHCreateItemFromParsingName, SHCreateStreamOnFileEx,
    ASSOCF, ASSOCSTR_SHELLEXTENSION,
};
use windows61::Win32::Graphics::DirectWrite::*;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, GetStockObject, InvalidateRect, BLACK_BRUSH, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::gfx::{self, Bitmap, Canvas, Gfx};
use super::{gdi, inside, pixels, png_pixels, put_mid, theme, w, wz, xy};

const VIDEO: &[&str] = &["mp4", "m4v", "mkv", "webm", "avi", "wmv", "asf", "mov", "mpg", "mpeg", "m2ts", "mts", "3gp", "3g2"];
const AUDIO: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "wma", "ogg", "oga", "opus", "aif", "aiff", "amr", "ac3"];
const IMAGES: &[&str] = &["jpg", "jpeg", "jfif", "png", "bmp", "webp", "tif", "tiff", "ico", "heic", "heif", "avif", "jxr", "dds", "tga", "psd"];
/// Shown as text even when Windows has a (white, plain) preview handler for them.
use crate::extract::{looks_text, OFFICE, TEXTS};
/// Text past this is cut, so opening a huge log stays instant.
const TEXT_MAX: usize = 256 * 1024;
const T_MEDIA: usize = 1;
const T_GIF: usize = 2;
/// Width (CSS px) of the divider strip at the pane's left.
pub const GAP: i32 = 8;

pub enum Target {
    Note(&'static str),
    File { path: String, name: String, detail: String },
}

enum Body {
    Note(&'static str),
    Picture,
    Text,
    Media,
    Handler,
    Anim, // animated GIF, decoded and played frame-by-frame ourselves
}

pub struct Preview {
    pub hwnd: HWND,
    video: HWND, // MFPlay draws the video into this child
    gfx: Gfx,
    cv: Canvas,
    mono: Option<gfx::Format>,
    scale: f64,
    key: String, // the path shown (or the note), so re-selecting the same file changes nothing
    title: String,
    detail: String,
    body: Body,
    note: &'static str, // under a fallback picture
    png: Option<Vec<u8>>,
    pic: Option<(Bitmap, i32, i32, u32, i32)>, // decoded for canvas `gen` at box size
    text: Vec<u16>,
    layout: Option<gfx::Text>,
    text_h: f32,
    scroll: f32,
    cut: bool,
    player: Option<IMFPMediaPlayer>,
    audio: bool,
    started: Instant,
    drag: Option<Bar>, // dragging the seek or volume bar
    vol: f32,          // 0–1, kept from file to file
    muted: bool,
    icons: Option<gfx::Format>, // Segoe MDL2 Assets: play, pause, speaker
    live: bool, // its divider is being dragged: stretch the picture, keep the text's wrap, until it's let go
    grip: f32,  // how lit the divider's grip is
    handler: Option<IPreviewHandler>,
    gif: Vec<(Vec<u8>, i32, i32, u32)>, // decoded frames: premultiplied BGRA, width, height, delay (ms)
    gif_i: usize,
    gif_bmp: Option<(Bitmap, u32)>, // current frame's bitmap, and the canvas `gen` it was made for
}

#[derive(Clone, Copy, PartialEq)]
enum Bar {
    Seek,
    Volume,
}

thread_local!(static PV: RefCell<Option<Preview>> = const { RefCell::new(None) });

/// Run `f` on the pane. `None` while it is busy further up the stack: a preview handler's cross-process call
/// pumps messages, so our window procedures can run inside it.
fn with<R>(f: impl FnOnce(&mut Preview) -> R) -> Option<R> {
    PV.with(|p| p.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

/// Make the (hidden) pane as a child of `parent`. Returns its window.
pub unsafe fn create(parent: HWND, scale: f64) -> HWND {
    static CLASS: std::sync::Once = std::sync::Once::new();
    let (pc, vc) = (wz("MosaicPreview"), wz("MosaicVideo"));
    let inst = GetModuleHandleW(null());
    CLASS.call_once(|| {
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.hInstance = inst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.lpfnWndProc = Some(preview_proc);
        wc.lpszClassName = pc.as_ptr();
        RegisterClassExW(&wc);
        wc.lpfnWndProc = Some(video_proc);
        wc.hbrBackground = GetStockObject(BLACK_BRUSH);
        wc.lpszClassName = vc.as_ptr();
        RegisterClassExW(&wc);
    });
    let hwnd = CreateWindowExW(0, pc.as_ptr(), null(), WS_CHILD | WS_CLIPCHILDREN, 0, 0, 10, 10, parent, null_mut(), inst, null());
    let video = CreateWindowExW(0, vc.as_ptr(), null(), WS_CHILD, 0, 0, 10, 10, hwnd, null_mut(), inst, null());
    if let Ok(gfx) = Gfx::new(scale, crate::i18n::lang() == "ar") {
        let p = Preview {
            hwnd,
            video,
            cv: Canvas::new(hwnd, &gfx),
            mono: mono(&gfx, scale),
            icons: icons(&gfx, scale),
            gfx,
            scale,
            key: String::new(),
            title: String::new(),
            detail: String::new(),
            body: Body::Note(""),
            note: "",
            png: None,
            pic: None,
            text: Vec::new(),
            layout: None,
            text_h: 0.0,
            scroll: 0.0,
            cut: false,
            player: None,
            audio: false,
            started: Instant::now(),
            drag: None,
            vol: 1.0,
            muted: false,
            live: false,
            grip: 0.0,
            handler: None,
            gif: Vec::new(),
            gif_i: 0,
            gif_bmp: None,
        };
        PV.with(|c| *c.borrow_mut() = Some(p));
    }
    hwnd
}

/// Show `t`. Returns (path, px) when the pane wants the shell's picture of a file at that size: the caller fetches
/// it off this thread and hands it to [`got_pic`].
pub fn show(t: Target) -> Option<(String, i32)> {
    with(|p| p.show(t)).flatten()
}

pub fn got_pic(path: &str, png: Option<Vec<u8>>) {
    with(|p| {
        if p.key == path && matches!(p.body, Body::Picture | Body::Media) {
            if png.is_none() && matches!(p.body, Body::Picture) {
                p.body = Body::Note("No preview");
            }
            p.png = png;
            p.pic = None;
            unsafe { InvalidateRect(p.hwnd, null(), 0) };
        }
    });
}

/// How lit the divider's grip is (0–1).
pub fn grip(a: f32) {
    with(|p| {
        if p.grip != a {
            p.grip = a;
            let r = RECT { left: 0, top: 0, right: p.px(GAP), bottom: 1 << 15 };
            unsafe { InvalidateRect(p.hwnd, &r, 0) };
        }
    });
}

/// The pane's divider is being dragged (`true`) or was let go.
pub fn resizing(on: bool) {
    with(|p| {
        p.live = on;
        if !on {
            p.relayout();
        }
    });
}

/// Stop playback and let go of the file (the pane was hidden).
pub fn clear() {
    with(|p| p.clear());
}

/// Redraw now: `theme()` is read fresh on every paint, but the pane is its own window, so the main
/// window's `InvalidateRect` never reaches it — a theme change needs this to show up without delay.
pub fn invalidate() {
    with(|p| unsafe { InvalidateRect(p.hwnd, null(), 0) });
}

pub fn rescale(scale: f64) {
    with(|p| {
        if let Ok(g) = Gfx::new(scale, crate::i18n::lang() == "ar") {
            p.scale = scale;
            p.cv = Canvas::new(p.hwnd, &g);
            p.mono = mono(&g, scale);
            p.icons = icons(&g, scale);
            p.gfx = g;
            p.pic = None;
            p.relayout();
        }
    });
}

/// Code font: Consolas 12px, wrapping at spaces (and inside words too long for a line), 4-space tabs.
fn mono(g: &Gfx, scale: f64) -> Option<gfx::Format> {
    unsafe {
        let f = g
            .dw
            .CreateTextFormat(windows61::core::w!("Consolas"), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, (12.0 * scale) as f32, windows61::core::w!(""))
            .ok()?;
        let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK);
        let _ = f.SetIncrementalTabStop((4.0 * 12.0 * 0.55 * scale) as f32); // Consolas is 0.55 em wide
        Some(f)
    }
}

/// The icon font Windows' own media controls use.
fn icons(g: &Gfx, scale: f64) -> Option<gfx::Format> {
    unsafe {
        g.dw.CreateTextFormat(windows61::core::w!("Segoe MDL2 Assets"), None, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, (14.0 * scale) as f32, windows61::core::w!(""))
            .ok()
    }
}

fn wr(r: &RECT) -> WinRect {
    WinRect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

/// Bytes as UTF-16 for on-screen display: `extract::decode_str` does the actual decoding.
fn decode(b: &[u8], cut: bool) -> Vec<u16> {
    crate::extract::decode_str(b, cut).encode_utf16().collect()
}

fn clock(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

impl Preview {
    fn px(&self, v: i32) -> i32 {
        (v as f64 * self.scale).round() as i32
    }

    fn show(&mut self, t: Target) -> Option<(String, i32)> {
        let key = match &t {
            Target::Note(m) => m.to_string(),
            Target::File { path, .. } => path.clone(),
        };
        if key == self.key {
            return None;
        }
        self.clear();
        self.key = key;
        let fetch = match t {
            Target::Note(m) => {
                self.body = Body::Note(m);
                None
            }
            Target::File { path, name, detail } => {
                self.title = name;
                self.detail = detail;
                self.open(&path)
            }
        };
        self.relayout();
        fetch
    }

    fn clear(&mut self) {
        unsafe {
            if let Some(p) = self.player.take() {
                let _ = p.Shutdown();
            }
            if let Some(h) = self.handler.take() {
                let _ = h.Unload();
            }
            ShowWindow(self.video, SW_HIDE);
            KillTimer(self.hwnd, T_MEDIA);
            KillTimer(self.hwnd, T_GIF);
            InvalidateRect(self.hwnd, null(), 0);
        }
        self.key.clear();
        self.title.clear();
        self.detail.clear();
        self.body = Body::Note("");
        self.note = "";
        self.png = None;
        self.pic = None;
        self.text.clear();
        self.layout = None;
        self.scroll = 0.0;
        self.cut = false;
        self.audio = false;
        self.gif.clear();
        self.gif_i = 0;
        self.gif_bmp = None;
    }

    /// Pick how to show `path`; returns a picture to fetch, if one is wanted.
    fn open(&mut self, path: &str) -> Option<(String, i32)> {
        let ext = std::path::Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let (body, _) = self.rects();
        let fetch = Some((path.to_string(), (body.right - body.left).max(body.bottom - body.top).clamp(64, 1600)));
        let e = ext.as_str();
        if e == "gif" {
            // Media Foundation has no real GIF source, so this is decoded and animated by hand
            if self.load_gif(path) {
                return None;
            }
            self.body = Body::Picture;
            return fetch;
        }
        if VIDEO.contains(&e) || AUDIO.contains(&e) {
            self.audio = AUDIO.contains(&e);
            if self.play(path) {
                return if self.audio { fetch } else { None }; // audio: the album art (or the file's icon)
            }
            self.note = "This file can't be played";
            self.body = Body::Picture;
            return fetch;
        }
        if IMAGES.contains(&e) {
            self.body = Body::Picture;
            return fetch;
        }
        if (TEXTS.contains(&e) && self.load_text(path, true)) || self.open_handler(path, e) || (OFFICE.contains(&e) && self.load_office_text(path, e)) || self.load_text(path, false) {
            return None;
        }
        self.note = "No preview for this type of file";
        self.body = Body::Picture;
        fetch
    }

    fn load_text(&mut self, path: &str, force: bool) -> bool {
        use std::io::Read;
        let mut b = Vec::new();
        let Ok(f) = std::fs::File::open(path) else { return false };
        if f.take(TEXT_MAX as u64 + 1).read_to_end(&mut b).is_err() || (!force && !looks_text(&b)) {
            return false;
        }
        self.cut = b.len() > TEXT_MAX;
        b.truncate(TEXT_MAX);
        self.text = decode(&b, self.cut);
        self.body = Body::Text;
        true
    }

    /// Falls back to plain text pulled out of an Office/OpenDocument zip when Windows has no real preview
    /// handler for it registered (no Office or LibreOffice installed) — a rough read beats "no preview".
    fn load_office_text(&mut self, path: &str, ext: &str) -> bool {
        let Some(text) = crate::extract::office_text(path, ext) else { return false };
        self.text = text.encode_utf16().collect();
        self.cut = false;
        self.body = Body::Text;
        true
    }

    /// Decodes every frame up front (GIFs previewed here are thumbnail-sized, not full movies) so playback is
    /// just picking the next one on a timer; `image` composites each frame onto the canvas per its disposal
    /// method, so every entry here is already a full straight-alpha RGBA frame at the GIF's own size.
    fn load_gif(&mut self, path: &str) -> bool {
        let Ok(f) = std::fs::File::open(path) else { return false };
        let Ok(dec) = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(f)) else { return false };
        let Ok(frames) = image::AnimationDecoder::into_frames(dec).collect_frames() else { return false };
        if frames.is_empty() {
            return false;
        }
        self.gif = frames
            .iter()
            .map(|fr| {
                let (n, d) = fr.delay().numer_denom_ms();
                // GIF delay is stored in centiseconds; 0 (and, by the convention every major browser follows, any
                // value under ~2 cs) means "as fast as the format allows" and is shown at a 10 fps default instead
                let ms = if d == 0 { 0 } else { n / d };
                let delay = if ms < 20 { 100 } else { ms };
                let buf = fr.buffer();
                let (w, h) = (buf.width() as usize, buf.height() as usize);
                let (px, dw, dh) = pixels(buf.as_raw(), w, h, w.max(h)); // premultiplies to BGRA; size = no downscale
                (px, dw, dh, delay)
            })
            .collect();
        self.gif_i = 0;
        self.gif_bmp = None;
        self.body = Body::Anim;
        unsafe { SetTimer(self.hwnd, T_GIF, self.gif[0].3, None) };
        true
    }

    /// Every frame's delay, once it's shown: advance to the next one and repaint.
    fn gif_tick(&mut self) {
        if self.gif.is_empty() {
            unsafe { KillTimer(self.hwnd, T_GIF) };
            return;
        }
        self.gif_i = (self.gif_i + 1) % self.gif.len();
        self.gif_bmp = None;
        unsafe {
            SetTimer(self.hwnd, T_GIF, self.gif[self.gif_i].3, None);
            InvalidateRect(self.hwnd, null(), 0);
        }
    }

    /// The preview handler registered for `.ext`, run by Windows in its preview host process.
    fn open_handler(&mut self, path: &str, ext: &str) -> bool {
        unsafe {
            let (dot, iid) = (wz(&format!(".{ext}")), wz("{8895b1c6-b41f-4c1c-a562-0d564250836f}"));
            let mut buf = [0u16; 64];
            let mut n = buf.len() as u32;
            if AssocQueryStringW(ASSOCF(0), ASSOCSTR_SHELLEXTENSION, PCWSTR(dot.as_ptr()), PCWSTR(iid.as_ptr()), PWSTR(buf.as_mut_ptr()), &mut n).is_err() {
                return false;
            }
            let Ok(clsid) = CLSIDFromString(PCWSTR(buf.as_ptr())) else { return false };
            // out of process only: an in-process handler that crashes would take the app with it
            let Ok(h) = CoCreateInstance::<_, IPreviewHandler>(&clsid, None, CLSCTX_LOCAL_SERVER) else { return false };
            let p = wz(path);
            let file = PCWSTR(p.as_ptr());
            let init = h.cast::<IInitializeWithItem>().is_ok_and(|i| SHCreateItemFromParsingName::<_, _, IShellItem>(file, None).and_then(|it| i.Initialize(&it, 0)).is_ok())
                || h.cast::<IInitializeWithFile>().is_ok_and(|i| i.Initialize(file, 0).is_ok())
                || h.cast::<IInitializeWithStream>().is_ok_and(|i| {
                    // STGM_READ | STGM_SHARE_DENY_NONE: the file stays usable while it is previewed
                    SHCreateStreamOnFileEx(file, 0x40, 0, BOOL(0), None::<&IStream>).and_then(|s| i.Initialize(&s, 0)).is_ok()
                });
            if !init {
                return false;
            }
            if let Ok(v) = h.cast::<IPreviewHandlerVisuals>() {
                let _ = v.SetBackgroundColor(COLORREF(gdi(theme().card)));
                let _ = v.SetTextColor(COLORREF(gdi(theme().text)));
            }
            self.body = Body::Handler;
            let (body, _) = self.rects();
            if h.SetWindow(WinHwnd(self.hwnd as _), &wr(&body)).is_err() || h.DoPreview().is_err() {
                let _ = h.Unload();
                return false;
            }
            self.handler = Some(h);
            true
        }
    }

    fn play(&mut self, path: &str) -> bool {
        static MF: std::sync::Once = std::sync::Once::new();
        unsafe {
            MF.call_once(|| {
                let _ = MFStartup(0x0002_0070, 0); // MF_VERSION, MFSTARTUP_FULL
            });
            let p = wz(path);
            let mut player = None;
            let made = MFPCreateMediaPlayer(PCWSTR(p.as_ptr()), BOOL(1), MFP_CREATION_OPTIONS(0), None::<&IMFPMediaPlayerCallback>, WinHwnd(self.video as _), Some(&mut player));
            let (Ok(()), Some(player)) = (made, player) else { return false };
            let _ = player.SetBorderColor(COLORREF(0));
            let _ = player.SetVolume(self.vol);
            let _ = player.SetMute(BOOL(self.muted as i32));
            self.player = Some(player);
            self.body = Body::Media;
            self.started = Instant::now();
            SetTimer(self.hwnd, T_MEDIA, 250, None);
        }
        true
    }

    /// Seconds played and in total, once the file is open.
    fn times(&self) -> Option<(f64, f64)> {
        let pl = self.player.as_ref()?;
        let secs = |v: &PROPVARIANT| u64::try_from(v).map(|t| t as f64).or_else(|_| i64::try_from(v).map(|t| t as f64)).ok().map(|t| t / 1e7);
        unsafe {
            let d = secs(&pl.GetDuration(&MFP_POSITIONTYPE_100NS).ok()?)?;
            let p = secs(&pl.GetPosition(&MFP_POSITIONTYPE_100NS).ok()?)?;
            Some((p.min(d), d))
        }
    }

    fn playing(&self) -> bool {
        self.player.as_ref().is_some_and(|p| unsafe { p.GetState() }.is_ok_and(|s| s == MFP_MEDIAPLAYER_STATE_PLAYING))
    }

    /// Distinct from a user's pause: MFPlay reaches this state on its own once playback runs off the end.
    fn stopped(&self) -> bool {
        self.player.as_ref().is_some_and(|p| unsafe { p.GetState() }.is_ok_and(|s| s == MFP_MEDIAPLAYER_STATE_STOPPED))
    }

    fn toggle(&mut self) {
        if let Some(p) = &self.player {
            unsafe {
                let _ = if self.playing() { p.Pause() } else { p.Play() };
                InvalidateRect(self.hwnd, null(), 0);
            }
        }
    }

    fn seek(&mut self, x: i32) {
        let (Some(p), Some((_, d))) = (&self.player, self.times()) else { return };
        let t = self.track();
        let f = ((x - t.left) as f64 / (t.right - t.left).max(1) as f64).clamp(0.0, 1.0);
        unsafe {
            let _ = p.SetPosition(&MFP_POSITIONTYPE_100NS, &PROPVARIANT::from((f * d * 1e7) as i64));
            InvalidateRect(self.hwnd, null(), 0);
        }
    }

    fn volume(&mut self, x: i32) {
        let t = self.vol_track();
        self.vol = ((x - t.left) as f32 / (t.right - t.left).max(1) as f32).clamp(0.0, 1.0);
        self.muted = false;
        self.apply_volume();
    }
    fn mute(&mut self) {
        self.muted = !self.muted;
        self.apply_volume();
    }
    fn apply_volume(&self) {
        if let Some(p) = &self.player {
            unsafe {
                let _ = p.SetVolume(self.vol);
                let _ = p.SetMute(BOOL(self.muted as i32));
            }
        }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    /// Every 250 ms while media is open: move the time along; give up on files Media Foundation can't open.
    fn tick(&mut self) {
        if self.player.is_none() {
            unsafe { KillTimer(self.hwnd, T_MEDIA) };
            return;
        }
        match self.times() {
            None if !self.playing() && self.started.elapsed() > Duration::from_secs(4) => {
                if let Some(p) = self.player.take() {
                    unsafe {
                        let _ = p.Shutdown();
                        ShowWindow(self.video, SW_HIDE);
                    }
                }
                self.body = Body::Note("This file can't be played");
            }
            // video ran off the end: MFPlay doesn't loop on its own, so restart it (audio just stops, as before)
            Some((_, dur)) if !self.audio && dur > 0.0 && self.stopped() => {
                if let Some(p) = &self.player {
                    unsafe {
                        let _ = p.SetPosition(&MFP_POSITIONTYPE_100NS, &PROPVARIANT::from(0i64));
                        let _ = p.Play();
                    }
                }
            }
            _ => {}
        }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    /// The pane's card: the window less the divider strip at its left (part of this window, so the grip moves in
    /// the same frame as the pane; its mouse input goes through to the main window).
    fn card(&self) -> RECT {
        let mut c: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut c) };
        RECT { left: self.px(GAP), ..c }
    }

    /// The content box and (for media) the control bar under it.
    fn rects(&self) -> (RECT, RECT) {
        let c = self.card();
        let (pad, lh) = (self.px(12), self.gfx.f.lh);
        let top = pad + 2 * lh + self.px(10);
        let mut body = RECT { left: c.left + pad, top, right: (c.right - pad).max(c.left + pad + 1), bottom: (c.bottom - pad).max(top + 1) };
        let mut ctl = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if matches!(self.body, Body::Media) {
            ctl = RECT { top: (body.bottom - lh - self.px(12)).max(body.top), ..body };
            body.bottom = (ctl.top - self.px(8)).max(body.top + 1);
        }
        (body, ctl)
    }
    fn play_btn(&self) -> RECT {
        let (_, c) = self.rects();
        RECT { right: c.left + (c.bottom - c.top), ..c }
    }
    /// Speaker (mute) button and volume bar at the bar's right end.
    fn mute_btn(&self) -> RECT {
        let (_, c) = self.rects();
        let x = self.vol_track().left - self.px(6);
        RECT { left: x - (c.bottom - c.top), right: x, ..c }
    }
    fn vol_track(&self) -> RECT {
        let (_, c) = self.rects();
        RECT { left: (c.right - self.px(64)).max(c.left), ..c }
    }
    /// The seek bar's clickable box: between ▶ and the time.
    fn track(&self) -> RECT {
        let (_, c) = self.rects();
        let tw = self.gfx.width("0:00:00 / 0:00:00", &self.gfx.f.reg);
        RECT { left: c.left + (c.bottom - c.top) + self.px(12), right: (self.mute_btn().left - tw - self.px(24)).max(c.left), ..c }
    }

    /// The pane changed size: move the video / handler, re-wrap text.
    fn relayout(&mut self) {
        let (body, _) = self.rects();
        unsafe {
            if self.player.is_some() && !self.audio {
                MoveWindow(self.video, body.left, body.top, body.right - body.left, body.bottom - body.top, 1);
                ShowWindow(self.video, SW_SHOWNA);
                if let Some(p) = &self.player {
                    let _ = p.UpdateVideo();
                }
            } else {
                ShowWindow(self.video, SW_HIDE);
            }
            if let Some(h) = &self.handler {
                let _ = h.SetRect(&wr(&body));
            }
            InvalidateRect(self.hwnd, null(), 0);
        }
        if !self.live {
            self.layout = None; // re-wrapped at the new width when painted (a long file takes a moment: not per drag step)
        }
    }

    fn text_layout(&mut self, width: i32) -> Option<gfx::Text> {
        if self.layout.is_none() && !self.text.is_empty() {
            let l = self.gfx.layout(&self.text, self.mono.as_ref()?, width as f32, 1e7)?;
            self.text_h = gfx::metrics(&l).height;
            self.layout = Some(l);
        }
        self.layout.clone()
    }

    fn wheel(&mut self, delta: i32) {
        if !matches!(self.body, Body::Text) {
            return;
        }
        let (body, _) = self.rects();
        let line = (12.0 * 1.17 * self.scale) as f32; // Consolas' line
        let max = (self.text_h - (body.bottom - body.top) as f32 + line).max(0.0);
        self.scroll = (self.scroll - delta as f32 / 120.0 * 3.0 * line).clamp(0.0, max);
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn paint(&mut self) {
        let (body, ctl) = self.rects();
        // decode the picture for this canvas and box (after a resize or a GPU reset, again)
        if let Some(png) = &self.png {
            let size = (body.right - body.left).max(body.bottom - body.top);
            if self.pic.as_ref().is_none_or(|p| p.3 != self.cv.gen || (p.4 != size && !self.live)) {
                self.pic = png_pixels(png, size).and_then(|(px, w, h)| self.cv.make_bitmap(&px, w, h).map(|b| (b, w, h, self.cv.gen, size)));
            }
        }
        if matches!(self.body, Body::Anim) && self.gif_bmp.as_ref().is_none_or(|b| b.1 != self.cv.gen) {
            if let Some((px, w, h, _)) = self.gif.get(self.gif_i) {
                self.gif_bmp = self.cv.make_bitmap(px, *w, *h).map(|b| (b, self.cv.gen));
            }
        }
        let layout = if matches!(self.body, Body::Text) { self.text_layout(body.right - body.left) } else { None };
        if !self.cv.begin() {
            return;
        }
        let (g, cv, lh, s) = (&self.gfx, &self.cv, self.gfx.f.lh, self.scale);
        let px = |v: i32| (v as f64 * s).round() as i32;
        let rad = |v: f64| (v * s) as f32;
        let c = self.card();
        cv.clear(theme().bg);
        cv.rrect(&c, rad(8.0), Some(theme().card), Some(theme().line));
        if self.grip > 0.0 {
            // the divider's grip: the middle quarter of the strip, lit by the main window (hover, drag)
            let (x, y, half) = (c.left / 2, (c.top + c.bottom) / 2, (c.bottom - c.top) / 8);
            cv.rrect_a(&RECT { left: x - px(2), top: y - half, right: x + px(2), bottom: y + half }, rad(2.0), Some(theme().accent), None, self.grip);
        }
        let bw = (body.right - body.left) as f32;
        if let Some(t) = g.layout(&w(&self.title), &g.f.semi, bw, lh as f32) {
            cv.text(&t, body.left, px(12), theme().text);
        }
        let detail = if self.cut { format!("{} · first {} KB shown", self.detail, TEXT_MAX / 1024) } else { self.detail.clone() };
        if let Some(t) = g.layout(&w(&detail), &g.f.reg, bw, lh as f32) {
            cv.text(&t, body.left, px(12) + lh, theme().muted);
        }
        let mid = |r: &RECT| (r.top + r.bottom - lh) / 2;
        match self.body {
            Body::Note(m) => put_mid(g, cv, m, &g.f.reg, &body, mid(&body), theme().muted),
            Body::Text => {
                if let Some(l) = &layout {
                    cv.clip(&body);
                    cv.text(l, body.left, body.top - self.scroll.round() as i32, theme().text);
                    cv.unclip();
                    // a thin scrollbar when it doesn't fit
                    let bh = (body.bottom - body.top) as f32;
                    if self.text_h > bh {
                        let len = (bh * bh / self.text_h).max(px(24) as f32);
                        let top = body.top as f32 + (bh - len) * self.scroll / (self.text_h - bh + (12.0 * 1.17 * s) as f32).max(1.0);
                        let r = RECT { left: c.right - px(7), top: top as i32, right: c.right - px(3), bottom: (top + len) as i32 };
                        cv.rrect_a(&r, rad(2.0), Some(theme().muted), None, 0.45);
                    }
                }
            }
            Body::Picture | Body::Media => {
                // the picture, contained and centered (for audio: the album art, else the file's icon)
                let bx = if self.note.is_empty() { body } else { RECT { bottom: body.bottom - 2 * lh, ..body } };
                if let Some((b, pw, ph, _, _)) = &self.pic {
                    if matches!(self.body, Body::Picture) || self.audio {
                        let k = ((bx.right - bx.left) as f64 / *pw as f64).min((bx.bottom - bx.top) as f64 / *ph as f64).min(1.0);
                        let (dw, dh) = ((*pw as f64 * k).round() as i32, (*ph as f64 * k).round() as i32);
                        let (x, y) = (bx.left + (bx.right - bx.left - dw) / 2, bx.top + (bx.bottom - bx.top - dh) / 2);
                        cv.bitmap(b, &RECT { left: x, top: y, right: x + dw, bottom: y + dh }, 1.0);
                    }
                }
                if !self.note.is_empty() {
                    put_mid(g, cv, self.note, &g.f.reg, &body, body.bottom - lh - px(4), theme().muted);
                }
            }
            Body::Handler => {}
            Body::Anim => {
                if let (Some((b, _)), Some((_, pw, ph, _))) = (&self.gif_bmp, self.gif.get(self.gif_i)) {
                    let k = ((body.right - body.left) as f64 / *pw as f64).min((body.bottom - body.top) as f64 / *ph as f64).min(1.0);
                    let (dw, dh) = ((*pw as f64 * k).round() as i32, (*ph as f64 * k).round() as i32);
                    let (x, y) = (body.left + (body.right - body.left - dw) / 2, body.top + (body.bottom - body.top - dh) / 2);
                    cv.bitmap(b, &RECT { left: x, top: y, right: x + dw, bottom: y + dh }, 1.0);
                }
            }
        }
        if matches!(self.body, Body::Media) {
            // ▶ / ❚❚, the seek bar, the time, then the speaker (mute) and the volume
            let glyph = |s: &str, fallback: &str, r: &RECT, c: u32| match &self.icons {
                Some(f) => {
                    if let Some(t) = g.layout(&w(s), f, 1e4, 1e4) {
                        let m = gfx::metrics(&t);
                        cv.text(&t, r.left + (r.right - r.left - m.width.round() as i32) / 2, r.top + (r.bottom - r.top - m.height.round() as i32) / 2, c);
                    }
                }
                None => put_mid(g, cv, fallback, &g.f.reg, r, mid(r), c),
            };
            let bar = |t: &RECT, f: f64| {
                let y = (t.top + t.bottom) / 2;
                let x = t.left + ((t.right - t.left) as f64 * f.clamp(0.0, 1.0)) as i32;
                cv.rrect_a(&RECT { left: t.left, top: y - px(2), right: t.right, bottom: y + px(2) }, rad(2.0), Some(theme().muted), None, 0.35);
                cv.rrect(&RECT { left: t.left, top: y - px(2), right: x.max(t.left + px(4)), bottom: y + px(2) }, rad(2.0), Some(theme().accent), None);
                cv.circle(x as f32, y as f32, rad(6.0), theme().accent, 1.0);
            };
            let b = self.play_btn();
            cv.rrect(&b, rad(6.0), Some(theme().bg), Some(theme().line));
            let playing = self.playing();
            glyph(if playing { "\u{E769}" } else { "\u{E768}" }, if playing { "❚❚" } else { "▶" }, &b, theme().text);
            let t = self.track();
            let (pos, dur) = self.times().unwrap_or((0.0, 0.0));
            bar(&t, if dur > 0.0 { pos / dur } else { 0.0 });
            let time = if dur > 0.0 { format!("{} / {}", clock(pos), clock(dur)) } else { "…".into() };
            super::put(g, cv, &time, &g.f.reg, t.right + px(12), mid(&ctl), theme().muted);
            let v = if self.muted { 0.0 } else { self.vol };
            let speaker = if v <= 0.0 { "\u{E74F}" } else if v < 0.34 { "\u{E993}" } else if v < 0.67 { "\u{E994}" } else { "\u{E995}" }; // mute, volume 1–3
            glyph(speaker, if v == 0.0 { "×" } else { "♪" }, &self.mute_btn(), if self.muted { theme().muted } else { theme().text });
            bar(&self.vol_track(), v as f64);
        }
        self.cv.end();
    }
}

unsafe extern "system" fn preview_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_ERASEBKGND => return 1,
        WM_NCHITTEST => {
            // the divider strip: the main window handles it (hover, drag)
            let mut pt = windows_sys::Win32::Foundation::POINT { x: (l & 0xffff) as i16 as i32, y: ((l >> 16) & 0xffff) as i16 as i32 };
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(h, &mut pt);
            let gap = (GAP as f64 * windows_sys::Win32::UI::HiDpi::GetDpiForWindow(h) as f64 / 96.0).round() as i32;
            return if pt.x < gap { HTTRANSPARENT as LRESULT } else { HTCLIENT as LRESULT };
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(h, &mut ps);
            with(|p| p.paint());
            EndPaint(h, &ps);
            return 0;
        }
        WM_SIZE => {
            with(|p| p.relayout());
            return 0;
        }
        WM_TIMER if wp == T_MEDIA => {
            with(|p| p.tick());
            return 0;
        }
        WM_TIMER if wp == T_GIF => {
            with(|p| p.gif_tick());
            return 0;
        }
        WM_MOUSEWHEEL => {
            with(|p| p.wheel(((wp >> 16) & 0xffff) as i16 as i32));
            return 0;
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(l);
            with(|p| {
                if p.player.is_none() {
                    return;
                }
                let wide = |t: RECT| RECT { left: t.left - p.px(8), right: t.right + p.px(8), ..t };
                if inside(&p.play_btn(), x, y) {
                    p.toggle();
                } else if inside(&p.mute_btn(), x, y) {
                    p.mute();
                } else if let Some(b) = [(Bar::Seek, p.track()), (Bar::Volume, p.vol_track())].into_iter().find(|(_, t)| inside(&wide(*t), x, y)).map(|b| b.0) {
                    p.drag = Some(b);
                    SetCapture(h);
                    if b == Bar::Seek { p.seek(x) } else { p.volume(x) }
                }
            });
            return 0;
        }
        WM_MOUSEMOVE => {
            let (x, _) = xy(l);
            with(|p| match p.drag {
                Some(Bar::Seek) => p.seek(x),
                Some(Bar::Volume) => p.volume(x),
                None => {}
            });
            return 0;
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            if with(|p| p.drag.take().is_some()) == Some(true) && m == WM_LBUTTONUP {
                ReleaseCapture();
            }
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}

/// The video's window: MFPlay repaints it; a click pauses or plays.
unsafe extern "system" fn video_proc(h: HWND, m: u32, wp: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            BeginPaint(h, &mut ps);
            with(|p| p.player.as_ref().map(|pl| pl.UpdateVideo()));
            EndPaint(h, &ps);
            return 0;
        }
        WM_LBUTTONDOWN => {
            with(|p| p.toggle());
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(h, m, wp, l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_told_from_binary_and_decoded() {
        assert!(looks_text(b"fn main() {}\n"));
        assert!(looks_text(&[0xff, 0xfe, b'h', 0, b'i', 0]));
        assert!(!looks_text(b"MZ\x90\0\x03\0"));
        assert_eq!(String::from_utf16_lossy(&decode(&[0xff, 0xfe, b'h', 0, b'i', 0], false)), "hi");
        assert_eq!(String::from_utf16_lossy(&decode("\u{feff}é".as_bytes(), false)), "é");
        assert_eq!(String::from_utf16_lossy(&decode(&[b'a', 0xe9], false)), "aé"); // not UTF-8: Latin-1
        assert_eq!(String::from_utf16_lossy(&decode(&"ab€".as_bytes()[..4], true)), "ab"); // cut inside the €
        assert_eq!(clock(75.0), "1:15");
        assert_eq!(clock(3725.0), "1:02:05");
    }
}
