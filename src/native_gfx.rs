//! Direct2D + DirectWrite drawing for the native window: antialiased shapes and browser-quality text. Render
//! targets run at 96 dpi, so every coordinate is a physical pixel and the UI scales its own sizes.

use std::ffi::c_void;

use windows61::core::{w, Result, PCWSTR};
use windows61::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND};
use windows61::Win32::Graphics::Direct2D::Common::*;
use windows61::Win32::Graphics::Direct2D::*;
use windows61::Win32::Graphics::DirectWrite::*;
use windows61::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows_numerics::Vector2;
use windows_sys::Win32::Foundation::RECT;

pub type Bitmap = ID2D1Bitmap;
pub type Text = IDWriteTextLayout;
pub type Brush = ID2D1SolidColorBrush;
pub type Format = IDWriteTextFormat;

/// Every render target gets a new number, so caches can tell one from the target it replaced.
static GEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

pub fn color(c: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: ((c >> 16) & 255) as f32 / 255.0, g: ((c >> 8) & 255) as f32 / 255.0, b: (c & 255) as f32 / 255.0, a }
}
fn rf(r: &RECT) -> D2D_RECT_F {
    D2D_RECT_F { left: r.left as f32, top: r.top as f32, right: r.right as f32, bottom: r.bottom as f32 }
}
fn v(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

pub struct Fonts {
    pub reg: IDWriteTextFormat,
    pub semi: IDWriteTextFormat,
    pub logo: IDWriteTextFormat,
    pub small: IDWriteTextFormat,
    pub label: IDWriteTextFormat, // grid names: two centered lines
    pub lh: i32,                  // line height of the 12 px text (CSS line-height 1.4)
}

pub struct Gfx {
    pub d2: ID2D1Factory,
    pub dw: IDWriteFactory,
    pub f: Fonts,
}

impl Gfx {
    /// `rtl`: build every format with `DWRITE_READING_DIRECTION_RIGHT_TO_LEFT` set, so DirectWrite shapes and
    /// bidi-reorders text automatically wherever it's used — the caller still positions the text's box itself
    /// (see `native.rs`'s `col_positions`), this only fixes how glyphs flow *within* that box.
    pub fn new(scale: f64, rtl: bool) -> Result<Gfx> {
        unsafe {
            let d2: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let f = fonts(&dw, scale as f32, rtl)?;
            Ok(Gfx { d2, dw, f })
        }
    }

    pub fn layout(&self, s: &[u16], f: &IDWriteTextFormat, width: f32, height: f32) -> Option<Text> {
        unsafe { self.dw.CreateTextLayout(s, f, width.max(0.0), height.max(0.0)).ok() }
    }

    /// Advance width of `s`, rounded up to whole pixels.
    pub fn width(&self, s: &str, f: &IDWriteTextFormat) -> i32 {
        let s: Vec<u16> = s.encode_utf16().collect();
        self.layout(&s, f, 1e5, 1e5).map_or(0, |t| metrics(&t).widthIncludingTrailingWhitespace.ceil() as i32)
    }
}

pub fn metrics(t: &Text) -> DWRITE_TEXT_METRICS {
    let mut m = DWRITE_TEXT_METRICS::default();
    unsafe {
        let _ = t.GetMetrics(&mut m);
    }
    m
}

/// Characters `a..b` of `t` bold and drawn with `brush` (a search match).
pub fn emphasize(t: &Text, a: usize, b: usize, brush: &Brush) {
    let r = DWRITE_TEXT_RANGE { startPosition: a as u32, length: (b - a) as u32 };
    unsafe {
        let _ = t.SetFontWeight(DWRITE_FONT_WEIGHT_BOLD, r);
        let _ = t.SetDrawingEffect(brush, r);
    }
}

unsafe fn fonts(dw: &IDWriteFactory, s: f32, rtl: bool) -> Result<Fonts> {
    let mk_in = |family: PCWSTR, px: f32, weight: i32, wrap: bool, segoe: bool| -> Result<IDWriteTextFormat> {
        let f = dw.CreateTextFormat(family, None, DWRITE_FONT_WEIGHT(weight), DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, px * s, w!(""))?;
        f.SetWordWrapping(if wrap { DWRITE_WORD_WRAPPING_EMERGENCY_BREAK } else { DWRITE_WORD_WRAPPING_NO_WRAP })?;
        if rtl {
            f.SetReadingDirection(DWRITE_READING_DIRECTION_RIGHT_TO_LEFT)?;
        }
        let sign = dw.CreateEllipsisTrimmingSign(&f)?;
        f.SetTrimming(&DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 }, &sign)?;
        if segoe {
            // CSS line-height 1.4 with the glyphs centered in the line box, as the web page lays them out
            let (size, lh) = (px * s, (px * 1.4 * s).round());
            let (asc, desc) = (size * 2210.0 / 2048.0, size * 514.0 / 2048.0); // Segoe UI's ascent/descent
            f.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, lh, (lh - asc - desc) / 2.0 + asc)?;
        }
        Ok(f)
    };
    let mk = |family: PCWSTR, px: f32, weight: i32, wrap: bool| mk_in(family, px, weight, wrap, true);
    let label = mk(w!("Segoe UI"), 12.0, 400, true)?;
    label.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
    Ok(Fonts {
        reg: mk(w!("Segoe UI"), 12.0, 400, false)?, // = Everything: Segoe UI 9 pt
        semi: mk(w!("Segoe UI"), 12.0, 600, false)?,
        logo: mk(w!("Segoe UI"), 20.0, 700, false)?,
        small: mk(w!("Segoe UI"), 9.0, 400, false)?,
        label,
        lh: (12.0 * 1.4 * s).round() as i32,
    })
}

/// One window's render target and the brushes drawn with it. Bitmaps and text effects belong to a target: when
/// the GPU resets, `gen` changes and whoever cached them drops theirs.
pub struct Canvas {
    hwnd: *mut c_void,
    d2: ID2D1Factory,
    rt: Option<ID2D1HwndRenderTarget>,
    brush: Option<Brush>,
    pub accent: Option<Brush>,
    round: Option<ID2D1StrokeStyle>,
    pub gen: u32,
}

impl Canvas {
    pub fn new(hwnd: *mut c_void, gfx: &Gfx) -> Canvas {
        let props = D2D1_STROKE_STYLE_PROPERTIES { startCap: D2D1_CAP_STYLE_ROUND, endCap: D2D1_CAP_STYLE_ROUND, lineJoin: D2D1_LINE_JOIN_ROUND, ..Default::default() };
        let round = unsafe { gfx.d2.CreateStrokeStyle(&props, None).ok() };
        Canvas { hwnd, d2: gfx.d2.clone(), rt: None, brush: None, accent: None, round, gen: 0 }
    }

    /// Make (or resize) the render target. False when there is none to draw with.
    pub fn ensure(&mut self) -> bool {
        let mut c: RECT = unsafe { std::mem::zeroed() };
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(self.hwnd, &mut c) };
        let size = D2D_SIZE_U { width: c.right.max(1) as u32, height: c.bottom.max(1) as u32 };
        unsafe {
            if let Some(rt) = &self.rt {
                let now = rt.GetPixelSize();
                if now.width != size.width || now.height != size.height {
                    let _ = rt.Resize(&size);
                }
                return true;
            }
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                // on the CPU: same antialiasing, and the GPU driver (~50 MB per process) never loads
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            // IMMEDIATELY: don't block on vsync here, the animation loop already paces frames with DwmFlush
            let hp = D2D1_HWND_RENDER_TARGET_PROPERTIES { hwnd: HWND(self.hwnd), pixelSize: size, presentOptions: D2D1_PRESENT_OPTIONS_IMMEDIATELY };
            let Ok(rt) = self.d2.CreateHwndRenderTarget(&props, &hp) else { return false };
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
            self.brush = rt.CreateSolidColorBrush(&color(0, 1.0), None).ok();
            self.accent = rt.CreateSolidColorBrush(&color(0x7aa2ff, 1.0), None).ok();
            self.rt = Some(rt);
            self.gen = GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        true
    }

    pub fn begin(&mut self) -> bool {
        if !self.ensure() {
            return false;
        }
        unsafe { self.rt().BeginDraw() };
        true
    }

    pub fn end(&mut self) {
        let Some(rt) = &self.rt else { return };
        if let Err(e) = unsafe { rt.EndDraw(None, None) } {
            if e.code() == D2DERR_RECREATE_TARGET {
                // the GPU was reset: start over with a new target (and new bitmaps) on the next paint
                self.rt = None;
                self.brush = None;
                self.accent = None;
                unsafe { windows_sys::Win32::Graphics::Gdi::InvalidateRect(self.hwnd, std::ptr::null(), 0) };
            }
        }
    }

    fn rt(&self) -> &ID2D1HwndRenderTarget {
        self.rt.as_ref().unwrap()
    }
    fn b(&self, c: u32, a: f32) -> &Brush {
        let b = self.brush.as_ref().unwrap();
        unsafe { b.SetColor(&color(c, a)) };
        b
    }

    pub fn clear(&self, c: u32) {
        unsafe { self.rt().Clear(Some(&color(c, 1.0))) };
    }

    pub fn fill(&self, r: &RECT, c: u32, a: f32) {
        unsafe { self.rt().FillRectangle(&rf(r), self.b(c, a)) };
    }

    /// Rounded box like a CSS one: background over the whole box, a 1 px border inside its edge.
    pub fn rrect(&self, r: &RECT, rad: f32, fill: Option<u32>, border: Option<u32>) {
        self.rrect_a(r, rad, fill, border, 1.0);
    }
    pub fn rrect_a(&self, r: &RECT, rad: f32, fill: Option<u32>, border: Option<u32>, a: f32) {
        unsafe {
            let rt = self.rt();
            if let Some(c) = fill {
                rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: rf(r), radiusX: rad, radiusY: rad }, self.b(c, a));
            }
            if let Some(c) = border {
                let mut i = rf(r);
                (i.left, i.top, i.right, i.bottom) = (i.left + 0.5, i.top + 0.5, i.right - 0.5, i.bottom - 0.5);
                let rad = (rad - 0.5).max(0.0);
                rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: i, radiusX: rad, radiusY: rad }, self.b(c, a), 1.0, None);
            }
        }
    }

    /// Box with its own radius per corner (top-left, top-right, bottom-right, bottom-left): a segmented control.
    pub fn corners(&self, r: &RECT, radii: [f32; 4], fill: u32, border: u32) {
        unsafe {
            if let Some(g) = self.path(rf(r), radii) {
                let _ = self.rt().FillGeometry(&g, self.b(fill, 1.0), None);
            }
            let mut i = rf(r);
            (i.left, i.top, i.right, i.bottom) = (i.left + 0.5, i.top + 0.5, i.right - 0.5, i.bottom - 0.5);
            if let Some(g) = self.path(i, radii.map(|x| (x - 0.5).max(0.0))) {
                self.rt().DrawGeometry(&g, self.b(border, 1.0), 1.0, None);
            }
        }
    }

    unsafe fn path(&self, r: D2D_RECT_F, [tl, tr, br, bl]: [f32; 4]) -> Option<ID2D1PathGeometry> {
        let g = self.d2.CreatePathGeometry().ok()?;
        let s = g.Open().ok()?;
        let arc = |x: f32, y: f32, rad: f32| {
            if rad > 0.0 {
                s.AddArc(&D2D1_ARC_SEGMENT { point: v(x, y), size: D2D_SIZE_F { width: rad, height: rad }, rotationAngle: 0.0, sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE, arcSize: D2D1_ARC_SIZE_SMALL });
            }
        };
        s.BeginFigure(v(r.left + tl, r.top), D2D1_FIGURE_BEGIN_FILLED);
        s.AddLine(v(r.right - tr, r.top));
        arc(r.right, r.top + tr, tr);
        s.AddLine(v(r.right, r.bottom - br));
        arc(r.right - br, r.bottom, br);
        s.AddLine(v(r.left + bl, r.bottom));
        arc(r.left, r.bottom - bl, bl);
        s.AddLine(v(r.left, r.top + tl));
        arc(r.left + tl, r.top, tl);
        s.EndFigure(D2D1_FIGURE_END_CLOSED);
        s.Close().ok()?;
        Some(g)
    }

    pub fn circle(&self, x: f32, y: f32, rad: f32, fill: u32, a: f32) {
        unsafe { self.rt().FillEllipse(&D2D1_ELLIPSE { point: v(x, y), radiusX: rad, radiusY: rad }, self.b(fill, a)) };
    }
    pub fn circle_line(&self, x: f32, y: f32, rad: f32, c: u32, width: f32) {
        unsafe { self.rt().DrawEllipse(&D2D1_ELLIPSE { point: v(x, y), radiusX: rad, radiusY: rad }, self.b(c, 1.0), width, None) };
    }

    /// A small chevron pointing down (or up), centered on (x, y), `half` px wide on each side.
    pub fn chevron(&self, x: f32, y: f32, half: f32, down: bool, c: u32, width: f32) {
        let d = if down { half / 2.0 } else { -half / 2.0 };
        unsafe {
            let Ok(g) = self.d2.CreatePathGeometry() else { return };
            let Ok(s) = g.Open() else { return };
            s.BeginFigure(v(x - half, y - d), D2D1_FIGURE_BEGIN_HOLLOW);
            s.AddLine(v(x, y + d));
            s.AddLine(v(x + half, y - d));
            s.EndFigure(D2D1_FIGURE_END_OPEN);
            if s.Close().is_ok() {
                self.rt().DrawGeometry(&g, self.b(c, 1.0), width, self.round.as_ref());
            }
        }
    }

    /// Text at a whole-pixel origin (crisp ClearType), clipped to its layout box.
    pub fn text(&self, t: &Text, x: i32, y: i32, c: u32) {
        let opts = D2D1_DRAW_TEXT_OPTIONS_CLIP | D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT;
        unsafe { self.rt().DrawTextLayout(v(x as f32, y as f32), t, self.b(c, 1.0), opts) };
    }

    /// Draw only inside `r` until [`Canvas::unclip`].
    pub fn clip(&self, r: &RECT) {
        unsafe { self.rt().PushAxisAlignedClip(&rf(r), D2D1_ANTIALIAS_MODE_ALIASED) };
    }
    pub fn unclip(&self) {
        unsafe { self.rt().PopAxisAlignedClip() };
    }

    pub fn bitmap(&self, b: &Bitmap, dst: &RECT, a: f32) {
        unsafe { self.rt().DrawBitmap(b, Some(&rf(dst)), a, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None) };
    }

    /// Premultiplied BGRA pixels as a bitmap of this target.
    pub fn make_bitmap(&mut self, px: &[u8], w: i32, h: i32) -> Option<Bitmap> {
        if !self.ensure() || w <= 0 || h <= 0 {
            return None;
        }
        let props = D2D1_BITMAP_PROPERTIES { pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED }, dpiX: 96.0, dpiY: 96.0 };
        unsafe { self.rt().CreateBitmap(D2D_SIZE_U { width: w as u32, height: h as u32 }, Some(px.as_ptr() as *const c_void), (w * 4) as u32, &props).ok() }
    }
}
