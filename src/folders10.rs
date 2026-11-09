//! View > Icons… Folder style: Windows 10's folders, drawn here (not Microsoft's icons, the same layout): the folder
//! stands upright, its flap on the left, what's inside standing slanted between back and flap; Desktop, Downloads,
//! Documents, Pictures, Music and Videos wear a badge, or (Windows 10 + 11) Windows 11's colours and white symbols.
//! [`icon`] answers for every folder `shell::thumbnail` is asked for, so tiles, rows, the sidebar and tabs all match.

use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::sync::OnceLock;

use resvg::tiny_skia::{FilterQuality, Paint, Pattern, Pixmap, Rect, SpreadMode, Transform};
use resvg::usvg;

/// 0 Windows' own folders, 1 Windows 10's, 2 Windows 10's with Windows 11's coloured special folders.
pub static STYLE: AtomicU8 = AtomicU8::new(0);

/// Where something stands inside the folder (in a 256 box): a parallelogram, its left and right edges upright, its
/// top and bottom dropping by `slope` per unit to the right. Measured off Windows 10's folders.
struct Spot {
    left: f32,
    right: f32,
    top: f32,
    height: f32,
    slope: f32,
}
const BACK: Spot = Spot { left: 76.5, right: 206.7, top: 38.0, height: 178.0, slope: 0.13 };
const FRONT: Spot = Spot { left: 61.7, right: 159.8, top: 39.0, height: 174.0, slope: 0.345 };
const PAGE: Spot = Spot { left: 120.0, right: 184.0, top: 66.0, height: 166.0, slope: 0.19 };

enum Card {
    Photo(Pixmap, bool), // a preview, cropped to fill; true: a video (a play button over it)
    Icon(Pixmap),        // no preview: the file's icon on a white card
    Page,                // a document without a preview: a page
}

enum Look {
    Closed,
    Special(usize), // Desktop, Downloads, Documents, Pictures, Music, Videos
    Folders,        // only folders inside
    Both(usize, Vec<(Card, &'static Spot)>), // View > Icons… Both: a special folder's own look, holding what's inside
    Contents(Vec<(Card, &'static Spot)>),
}

/// The folder at `path` as this style draws it, a PNG `size` px square; None when Windows' own is wanted (the
/// style is off, or `path` is no folder, or a drive).
pub fn icon(path: &str, size: i32, icon_only: bool) -> Option<Vec<u8>> {
    let style = STYLE.load(Relaxed);
    let p = std::path::Path::new(path);
    if style == 0 || p.parent().is_none() || !std::fs::metadata(p).is_ok_and(|m| m.is_dir()) {
        return None;
    }
    let look = match special(path) {
        Some(k) => Look::Special(k),
        None if icon_only => Look::Closed,
        None => contents(p, size),
    };
    png(&render(&look, style, size)?)
}

/// The closed folder, for a tile still waiting on its contents; None when the style is off.
pub fn closed(size: i32) -> Option<Vec<u8>> {
    let style = STYLE.load(Relaxed);
    (style != 0).then(|| render(&Look::Closed, style, size).and_then(|p| png(&p))).flatten()
}

/// View > Icons…: how `style` shows a folder of pictures (`photo` a PNG inside it; Windows 10 + 11: Pictures).
pub fn preview(style: u8, size: i32, photo: &[u8]) -> Option<Vec<u8>> {
    let look = if style == 2 { Look::Special(3) } else { Look::Contents(vec![(Card::Photo(Pixmap::decode_png(photo).ok()?, false), &BACK)]) };
    png(&render(&look, style, size)?)
}

/// View > Icons… Both: the special folder `kind_dir` (Pictures, Videos...) as this style draws it, holding what
/// `dir` holds; that folder alone when `dir` shows nothing inside. None when the style is off.
pub fn both(kind_dir: &str, dir: &str, size: i32) -> Option<Vec<u8>> {
    let style = STYLE.load(Relaxed);
    let k = special(kind_dir).filter(|_| style != 0)?;
    let look = match contents(std::path::Path::new(dir), size) {
        Look::Contents(cards) => Look::Both(k, cards),
        _ => Look::Special(k),
    };
    png(&render(&look, style, size)?)
}

/// [`both`] for View > Icons…' choices: Pictures holding `photo`.
pub fn preview_both(style: u8, size: i32, photo: &[u8]) -> Option<Vec<u8>> {
    png(&render(&Look::Both(3, vec![(Card::Photo(Pixmap::decode_png(photo).ok()?, false), &BACK)]), style, size)?)
}

fn png(p: &Pixmap) -> Option<Vec<u8>> {
    p.encode_png().ok()
}

/// Which special folder `path` is, wherever it was moved to.
fn special(path: &str) -> Option<usize> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::*;
    static DIRS: OnceLock<Vec<Option<String>>> = OnceLock::new();
    let dirs = DIRS.get_or_init(|| {
        [&FOLDERID_Desktop, &FOLDERID_Downloads, &FOLDERID_Documents, &FOLDERID_Pictures, &FOLDERID_Music, &FOLDERID_Videos]
            .into_iter()
            .map(|id| unsafe {
                let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, HANDLE::default()).ok()?;
                let s = p.to_string().ok();
                CoTaskMemFree(Some(p.0 as _));
                s.map(|s| s.trim_end_matches('\\').to_lowercase())
            })
            .collect()
    });
    let path = path.trim_end_matches('\\').to_lowercase();
    dirs.iter().position(|d| d.as_deref() == Some(path.as_str()))
}

/// What Windows 10 shows inside a folder: its first two files (hidden ones skipped, a look at no more than 50
/// entries), or a folder when it holds only folders.
fn contents(dir: &std::path::Path, size: i32) -> Look {
    use std::os::windows::fs::MetadataExt;
    let (mut files, mut sub) = (Vec::new(), false);
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten().take(50) {
        let Ok(m) = e.metadata() else { continue };
        if m.file_attributes() & 0x6 != 0 {
            continue; // hidden or system: desktop.ini, Thumbs.db
        }
        if m.is_dir() {
            sub = true;
        } else if files.len() < 2 {
            files.push(e.path());
            if files.len() == 2 {
                break;
            }
        }
    }
    let card = |f: &std::path::PathBuf| -> Option<Card> {
        let name = f.to_string_lossy();
        let pic = crate::shell::windows_thumbnail(&name, size, false)?;
        let is_icon = crate::shell::windows_thumbnail(&name, size, true).as_ref() == Some(&pic);
        let kind = crate::kind_of(&name);
        Some(match (is_icon, kind) {
            (true, 4) => Card::Page,
            (true, _) => Card::Icon(Pixmap::decode_png(&pic).ok()?),
            _ => Card::Photo(Pixmap::decode_png(&pic).ok()?, kind == 2),
        })
    };
    // one file stands at the back; with two, the first in front of the second
    let cards: Vec<_> = match files.as_slice() {
        [a] => card(a).map(|a| (a, &BACK)).into_iter().collect(),
        [a, b] => [card(b).map(|b| (b, &BACK)), card(a).map(|a| (a, &FRONT))].into_iter().flatten().collect(),
        _ => Vec::new(),
    };
    match (cards.is_empty(), files.is_empty() && sub) {
        (_, true) => Look::Folders,
        (true, _) => Look::Closed,
        _ => Look::Contents(cards),
    }
}

// ---- drawing: SVG in a 256 box, as the design was made; previews are painted between its layers ----

/// Back top, back bottom, flap top, flap bottom, the flap's shadow; and whether the back fades down-right (else up).
pub(crate) struct Palette([&'static str; 5], bool);
pub(crate) const YELLOW: Palette = Palette(["#d9b052", "#fbdc80", "#fbd977", "#ffe8a2", "#7a5200"], false);

const BACK_PATH: &str = "M48 28 H174 Q177 28 177 31 V139 L188 151 V209 Q188 212 185 212 H48 Q45 212 45 209 V31 Q45 28 48 28 Z";
const FLAP_PATH: &str = "M45 31 Q45 28 48 30 L90 66 Q93 69 93 72 L89 242 Q89 247 85 244 L47 213 Q45 211 45 208 Z";

fn defs(p: &Palette) -> String {
    let [bt, bb, ft, fb, sh] = p.0;
    let dir = if p.1 { r#"x1="0" y1="0" x2="1" y2="1""# } else { r#"x1="0" y1="1" x2="1" y2="0""# };
    format!(
        r##"<defs><linearGradient id="b" {dir}><stop offset="0" stop-color="{bt}"/><stop offset="1" stop-color="{bb}"/></linearGradient>
        <linearGradient id="f" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{ft}"/><stop offset="1" stop-color="{fb}"/></linearGradient>
        <linearGradient id="s" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{sh}" stop-opacity=".14"/><stop offset="1" stop-color="{sh}" stop-opacity="0"/></linearGradient></defs>"##
    )
}
pub(crate) fn back(p: &Palette) -> String {
    format!(r#"{}<path d="{BACK_PATH}" fill="url(#b)"/>"#, defs(p))
}
pub(crate) fn flap(p: &Palette) -> String {
    format!(
        r##"{}<path d="M93 72 L110 86 V212 H93 Z" fill="url(#s)"/><path d="{FLAP_PATH}" fill="url(#f)"/>
        <path d="M91 70 L87.5 241" stroke="#fff" stroke-opacity=".6" stroke-width="2" stroke-linecap="round"/>"##,
        defs(p)
    )
}

/// `inner` drawn standing at `s`, in a box its own width × height.
fn at(s: &Spot, inner: &str) -> String {
    format!(r#"<g transform="translate({} {}) matrix(1 {} 0 1 0 0)">{inner}</g>"#, s.left, s.top, s.slope)
}

/// The document's little picture: blue sky, a cloud, blue hills, water.
pub(crate) fn blue_pic(x: f32, y: f32, w: f32, h: f32, id: &str) -> String {
    format!(
        r##"<defs><linearGradient id="{id}" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#3f9be6"/><stop offset="1" stop-color="#bfe2fa"/></linearGradient></defs>
        <rect x="{x}" y="{y}" width="{w}" height="{h}" fill="url(#{id})"/>
        <ellipse cx="{}" cy="{}" rx="{}" ry="{}" fill="#fff" opacity=".85"/>
        <path d="M{x} {} Q{} {} {} {} T{} {} V{} H{x} Z" fill="#2f6fa3"/>
        <rect x="{x}" y="{}" width="{w}" height="{}" fill="#3d86c2"/>
        <rect x="{x}" y="{y}" width="{w}" height="{h}" fill="none" stroke="#6e8aa3" stroke-width=".8"/>"##,
        x + w * 0.3, y + h * 0.28, w * 0.2, h * 0.1,
        y + h * 0.62, x + w * 0.35, y + h * 0.4, x + w * 0.65, y + h * 0.6, x + w, y + h * 0.58, y + h,
        y + h * 0.76, h * 0.24,
    )
}

/// A document page `w` × `h`: maybe a folded corner, a big blue italic A, blue heading lines, a picture, grey lines.
fn page(w: f32, h: f32, fold: bool, id: &str) -> String {
    let c = if fold { w * 0.2 } else { 0.0 };
    let line = |x1: f32, x2: f32, y: f32, col: &str, t: f32| format!(r#"<rect x="{}" y="{}" width="{}" height="{}" fill="{col}"/>"#, w * x1, h * y, w * (x2 - x1), h * t);
    let mut out = format!(r##"<path d="M0 0 H{} L{w} {c} V{h} H0 Z" fill="#fbfcfd" stroke="#a9b0b8" stroke-width="1.2"/>"##, w - c);
    if fold {
        out += &format!(r##"<path d="M{} 0 V{c} H{w} Z" fill="#e3e7ec" stroke="#a9b0b8" stroke-width="1.2"/>"##, w - c);
    }
    // the letter as a shape (no fonts here): 1 = its size, standing on 0
    out += &format!(
        r##"<g transform="translate({} {}) scale({})"><path fill="#1683d8" fill-rule="evenodd" d="M0 0 L0.4 -0.7 H0.47 L0.58 0 H0.47 L0.445 -0.2 H0.2 L0.09 0 Z M0.25 -0.28 L0.43 -0.58 L0.44 -0.28 Z M-0.05 0 H0.15 V-0.03 H-0.05 Z M0.4 0 H0.64 V-0.03 H0.4 Z"/></g>"##,
        w * 0.07, h * 0.32, h * 0.27
    );
    for (x1, x2, y) in [(0.36, 0.72, 0.1), (0.36, 0.72, 0.17), (0.36, 0.92, 0.24), (0.36, 0.92, 0.31)] {
        out += &line(x1, x2, y, "#3a9be0", 0.006);
    }
    out += &line(0.08, 0.92, 0.39, "#1683d8", 0.014);
    for y in [0.47, 0.54, 0.61] {
        out += &line(0.08, 0.42, y, "#b5bac0", 0.006);
    }
    out += &blue_pic(w * 0.47, h * 0.45, w * 0.45, h * 0.17, id);
    for y in [0.69, 0.76, 0.83, 0.9] {
        out += &line(0.08, 0.92, y, "#b5bac0", 0.006);
    }
    out
}

/// Folder of folders: a second folder behind the first, offset right, a page sticking out of it.
fn folders() -> String {
    let p = &YELLOW;
    format!(
        r##"{}<defs>
        <linearGradient id="m" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#eccd6e"/><stop offset=".35" stop-color="#ffe89f"/><stop offset="1" stop-color="#ffe69b"/></linearGradient>
        <linearGradient id="p" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#ffeaa6"/><stop offset="1" stop-color="#ffe28d"/></linearGradient>
        <linearGradient id="r" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#f1d57b"/><stop offset="1" stop-color="#fddf80"/></linearGradient>
        <linearGradient id="d" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#000" stop-opacity=".16"/><stop offset=".6" stop-color="#000" stop-opacity="0"/></linearGradient></defs>
        <path d="M73 46 H189 Q192 46 192 49 V160 L206 173 V229 Q206 232 203 232 H73 Z" fill="url(#r)"/>
        {}
        <path d="M68 37 Q68 33 72 34 L148 59 Q151 60 151 64 V166 L146 173 V237 Q146 241 142 240 L94 229 L68 200 Z" fill="url(#m)"/>
        <path d="M93 72 L104 80 V212 H93 Z" fill="url(#s)"/>
        <path d="M45 31 Q45 28 48 30 L90 63 Q93 65 93 68 V180 L89 186 V242 Q89 247 85 244 L47 213 Q45 211 45 208 Z" fill="url(#p)"/>"##,
        back(p),
        at(&PAGE, &format!(r#"{}<rect width="64" height="166" fill="url(#d)"/>"#, page(64.0, 166.0, false, "q")))
    )
}

/// Windows 10's special folders: the closed folder, a badge in its bottom-right corner.
fn badge(k: usize) -> String {
    match k {
        0 => r##"<defs><linearGradient id="k" x1="0" y1="1" x2="1" y2="0"><stop offset="0" stop-color="#0a7fe0"/><stop offset=".55" stop-color="#22a6ff"/><stop offset=".56" stop-color="#3cb2ff"/><stop offset="1" stop-color="#5cc2ff"/></linearGradient></defs>
            <rect x="108" y="128" width="132" height="96" fill="url(#k)" stroke="#0a68b8" stroke-width="1.5"/>
            <rect x="108" y="216" width="132" height="8" fill="#0b5fa8"/><rect x="112" y="218.5" width="22" height="3" fill="#9fd6ff"/>
            <rect x="226" y="218.5" width="3" height="3" fill="#fff"/><rect x="232" y="218.5" width="3" height="3" fill="#fff"/>"##.into(),
        1 => r##"<defs><linearGradient id="k" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#5db4f2"/><stop offset=".5" stop-color="#3a95e4"/><stop offset=".5" stop-color="#2a82d6"/><stop offset="1" stop-color="#1e72c6"/></linearGradient></defs>
            <path d="M152 104 H200 V166 H236 L176 228 L116 166 H152 Z" fill="url(#k)" stroke="#1a64b0" stroke-width="1.5" stroke-linejoin="round"/>"##.into(),
        2 => format!(r#"<g transform="translate(113 73)">{}</g>"#, page(111.0, 151.0, true, "q")),
        3 => format!(r##"<rect x="108" y="124" width="135" height="100" fill="#fff" stroke="#9aa3ad" stroke-width="1.5"/>{}"##, blue_pic(115.0, 131.0, 121.0, 86.0, "q")),
        4 => r##"<defs><linearGradient id="k" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#3fb0ff"/><stop offset="1" stop-color="#0a6fd0"/></linearGradient></defs>
            <path d="M180 40 Q186 78 210 98 Q240 126 226 172 Q230 132 194 116 V206 Q194 236 160 246 Q126 254 118 232 Q112 210 142 196 Q164 186 180 192 Z" fill="url(#k)"/>"##.into(),
        _ => {
            let holes: String = (0..8).map(|i| 92 + 16 * i).map(|y| format!(r##"<rect x="109" y="{y}" width="8" height="9" rx="1.5" fill="#e8e8e8"/><rect x="218" y="{y}" width="8" height="9" rx="1.5" fill="#e8e8e8"/>"##)).collect();
            format!(r##"<rect x="104" y="84" width="127" height="143" fill="#2b2b2b"/>{holes}{}{}"##, blue_pic(123.0, 92.0, 89.0, 60.0, "q"), blue_pic(123.0, 159.0, 89.0, 60.0, "r"))
        }
    }
}

/// Windows 10 + 11's special folders: Windows 11's colours (flap top, flap bottom, back top, back bottom, shadow)
/// and white symbol, centred on the whole back and partly behind the flap; and what goes under it unmoved.
fn coloured(k: usize) -> (Palette, String, &'static str) {
    const W: &str = "#fbfdff";
    let (c, glyph, under): ([&'static str; 5], String, &'static str) = match k {
        0 => (["#7fe9ee", "#22a2cf", "#3cc3dd", "#0a6fbf", "#043a66"],
            format!(r#"<rect x="108" y="74" width="16" height="16" rx="4" fill="{W}"/><rect x="108" y="98" width="16" height="16" rx="4" fill="{W}"/>"#),
            r##"<path d="M45 194 H188 V209 Q188 212 185 212 H48 Q45 212 45 209 Z" fill="#084163"/><circle cx="168" cy="203" r="3.5" fill="#fbfdff"/><circle cx="178" cy="203" r="3.5" fill="#fbfdff"/>"##),
        1 => (["#74dcb2", "#14ab9c", "#2cbb9c", "#019aa6", "#024a3a"],
            format!(r#"<path d="M138 90 V146 M116 124 L138 146 L160 124 M114 166 H162" fill="none" stroke="{W}" stroke-width="9" stroke-linecap="round" stroke-linejoin="round"/>"#), ""),
        2 => (["#ccd8e4", "#90abc6", "#a3bad0", "#6785a3", "#3a4f60"],
            format!(r#"<rect x="108" y="96" width="30" height="9" rx="4.5" fill="{W}"/><rect x="108" y="112" width="30" height="9" rx="4.5" fill="{W}"/><rect x="146" y="94" width="26" height="27" rx="6" fill="{W}"/><rect x="108" y="130" width="64" height="9" rx="4.5" fill="{W}"/><rect x="108" y="146" width="64" height="9" rx="4.5" fill="{W}"/>"#), ""),
        3 => (["#6ac8f1", "#1384ce", "#2ba5e1", "#0669bc", "#023d70"],
            format!(r##"<defs><linearGradient id="t" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#fbfdff"/><stop offset="1" stop-color="#d3e1ff"/></linearGradient></defs><circle cx="158" cy="96" r="10" fill="{W}"/><path d="M82 212 L134 136 Q138 130 142 136 L194 212 Z" fill="url(#t)"/>"##), ""),
        4 => (["#fcae78", "#e3806a", "#e5876f", "#a96aa5", "#6a3060"],
            format!(r#"<path d="M140 158 V94 Q140 87 147 90 L163 97 Q170 100 170 108 V114 Q170 120 164 117 L149 111 V158 Z" fill="{W}"/><circle cx="132" cy="158" r="17" fill="{W}"/>"#), ""),
        _ => (["#d0a6fa", "#a067f2", "#b97ff6", "#8e4beb", "#3c2a62"],
            format!(r##"<path d="M126 106 Q126 97 134 102 L153 115 Q159 120 153 125 L134 138 Q126 143 126 134 Z" fill="{W}"/>{}"##,
                [96, 113, 130].map(|y| format!(r##"<rect x="104" y="{y}" width="11" height="13" rx="3" fill="#3c2a62" opacity=".75"/><rect x="161" y="{y}" width="11" height="13" rx="3" fill="#3c2a62" opacity=".75"/>"##)).concat()), ""),
    };
    // the palette lists flap colours first; `Palette` wants the back's first
    (Palette([c[2], c[3], c[0], c[1], c[4]], true), glyph, under)
}

enum Layer {
    Svg(String),
    Card(Card, &'static Spot),
}

fn render(look: &Look, style: u8, size: i32) -> Option<Pixmap> {
    let y = &YELLOW;
    let layers = match look {
        Look::Closed => vec![Layer::Svg(back(y) + &flap(y))],
        Look::Folders => vec![Layer::Svg(folders())],
        Look::Special(k) if style == 2 => {
            let (p, glyph, under) = coloured(*k);
            vec![Layer::Svg(format!(r#"{}{under}<g transform="translate(-22 0)">{glyph}</g>{}"#, back(&p), flap(&p)))]
        }
        Look::Special(k) => vec![Layer::Svg(back(y) + &flap(y) + &badge(*k))],
        Look::Contents(cards) => holding(back(y), cards, flap(y)),
        // the kind's symbol printed on the flap: Windows 10's badge, or Windows 10 + 11's colours and white symbol
        Look::Both(k, cards) if style == 2 => {
            let (p, glyph, under) = coloured(*k);
            // Pictures' mountain stands on the flap's bottom edge, across it, as on the folder; the rest in its middle
            let at = if *k == 3 { (42.0, 100.0, 190.0) } else { (34.0, 64.0, 145.0) };
            holding(back(&p) + under, cards, flap(&p) + &on_flap(&glyph, GLYPH_BOX[*k], at))
        }
        Look::Both(k, cards) => holding(back(y), cards, flap(y) + &on_flap(&badge(*k), BADGE_BOX[*k], (34.0, 64.0, 150.0))),
    };
    let mut pm = Pixmap::new(size as u32, size as u32)?;
    let k = size as f32 / 256.0;
    for l in layers {
        match l {
            Layer::Svg(body) => {
                let svg = format!(r#"<svg viewBox="0 0 256 256" width="256" height="256" xmlns="http://www.w3.org/2000/svg">{body}</svg>"#);
                let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
                resvg::render(&tree, Transform::from_scale(k, k), &mut pm.as_mut());
            }
            Layer::Card(c, s) => paint_card(&mut pm, &c, s, k),
        }
    }
    Some(pm)
}

/// Each special folder's badge ([`badge`]) and symbol ([`coloured`]): left, top, right, bottom in the 256 box.
const BADGE_BOX: [[f32; 4]; 6] = [[108.0, 128.0, 240.0, 224.0], [116.0, 104.0, 236.0, 228.0], [113.0, 73.0, 224.0, 224.0], [108.0, 124.0, 243.0, 224.0], [118.0, 40.0, 230.0, 250.0], [104.0, 84.0, 231.0, 227.0]];
const GLYPH_BOX: [[f32; 4]; 6] = [[108.0, 74.0, 124.0, 114.0], [114.0, 90.0, 162.0, 166.0], [108.0, 94.0, 172.0, 155.0], [82.0, 86.0, 194.0, 212.0], [115.0, 87.0, 170.0, 175.0], [104.0, 96.0, 172.0, 143.0]];

/// `art` (within `bx`) printed on the flap, cut by its edges: fitted within `at`'s width and height, its middle at
/// `at`'s height, narrowed and sheared down to the right as the flap, turned away, is.
fn on_flap(art: &str, bx: [f32; 4], at: (f32, f32, f32)) -> String {
    const SLOPE: f32 = 0.84; // the flap's top and bottom edges drop this much per unit right ([`FLAP_PATH`])
    const NARROW: f32 = 0.55; // how much narrower the flap's turn makes what's on it
    let (w, h) = (bx[2] - bx[0], bx[3] - bx[1]);
    let s = (at.0 / (w * NARROW)).min(at.1 / h);
    let sx = s * NARROW;
    let (cx, cy) = ((bx[0] + bx[2]) / 2.0, (bx[1] + bx[3]) / 2.0);
    format!(
        r#"<clipPath id="fc"><path d="{FLAP_PATH}"/></clipPath><g clip-path="url(#fc)"><g transform="matrix({sx} {} 0 {s} 68 {}) translate({} {})">{art}</g></g>"#,
        sx * SLOPE,
        at.2,
        -cx,
        -cy
    )
}

/// A folder's `back`, what's inside standing on it, its `front` over them.
fn holding(back: String, cards: &[(Card, &'static Spot)], front: String) -> Vec<Layer> {
    let mut v = vec![Layer::Svg(back)];
    for (c, s) in cards {
        v.push(match c {
            Card::Page => Layer::Svg(at(s, &page(s.right - s.left, s.height, true, "q"))),
            Card::Photo(p, video) => Layer::Card(Card::Photo(p.clone(), *video), s),
            Card::Icon(p) => Layer::Card(Card::Icon(p.clone()), s),
        });
    }
    v.push(Layer::Svg(front));
    v
}

/// A preview standing at `s` (`k` px per unit): a photo cropped to fill it, or an icon large on a white card,
/// cut by its edges as Windows 10 does; then its outline, and a play button on a video.
fn paint_card(pm: &mut Pixmap, c: &Card, s: &Spot, k: f32) {
    let (w, h) = (s.right - s.left, s.height);
    let place = Transform::from_row(k, k * s.slope, 0.0, k, k * s.left, k * s.top);
    let fill = |pm: &mut Pixmap, img: &Pixmap, scale: f32, ox: f32, oy: f32, r: Rect| {
        let shader = Pattern::new(img.as_ref(), SpreadMode::Pad, FilterQuality::Bicubic, 1.0, Transform::from_row(scale, 0.0, 0.0, scale, ox, oy));
        pm.fill_rect(r, &Paint { shader, anti_alias: true, ..Default::default() }, place, None);
    };
    let card = Rect::from_xywh(0.0, 0.0, w, h);
    let mut extra = String::new();
    match (c, card) {
        (Card::Photo(img, video), Some(r)) => {
            let (iw, ih) = (img.width() as f32, img.height() as f32);
            let scale = (w / iw).max(h / ih);
            fill(pm, img, scale, (w - iw * scale) / 2.0, (h - ih * scale) / 2.0, r);
            if *video {
                let (x, y) = (w / 2.0, h / 2.0);
                extra = format!(r##"<circle cx="{x}" cy="{y}" r="17" fill="rgba(0,0,0,.45)"/><path d="M{} {} L{} {y} L{} {} Z" fill="#fff"/>"##, x - 6.0, y - 10.0, x + 10.0, x - 6.0, y + 10.0);
            }
        }
        (Card::Icon(img), Some(r)) => {
            pm.fill_rect(r, &Paint { shader: resvg::tiny_skia::Shader::SolidColor(resvg::tiny_skia::Color::WHITE), anti_alias: true, ..Default::default() }, place, None);
            let side = h * 0.93;
            let scale = side / img.width().max(img.height()) as f32;
            let (ox, oy) = ((w - img.width() as f32 * scale) / 2.0, (h - img.height() as f32 * scale) / 2.0);
            if let Some(r) = Rect::from_ltrb(ox.max(0.0), oy.max(0.0), (ox + img.width() as f32 * scale).min(w), (oy + img.height() as f32 * scale).min(h)) {
                fill(pm, img, scale, ox, oy, r);
            }
        }
        _ => {}
    }
    let edge = format!(r#"<rect x=".5" y=".5" width="{}" height="{}" fill="none" stroke="rgba(0,0,0,.18)"/>{extra}"#, w - 1.0, h - 1.0);
    let svg = format!(r#"<svg viewBox="0 0 256 256" width="256" height="256" xmlns="http://www.w3.org/2000/svg">{}</svg>"#, at(s, &edge));
    if let Ok(tree) = usvg::Tree::from_str(&svg, &usvg::Options::default()) {
        resvg::render(&tree, Transform::from_scale(k, k), &mut pm.as_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every look draws: something opaque where the folder's back is, nothing in the empty top-left corner.
    #[test]
    fn every_look_draws() {
        let photo = {
            let mut p = Pixmap::new(40, 30).unwrap();
            p.fill(resvg::tiny_skia::Color::from_rgba8(200, 30, 30, 255));
            p
        };
        let mut looks = vec![Look::Closed, Look::Folders, Look::Contents(vec![(Card::Photo(photo.clone(), true), &BACK), (Card::Page, &FRONT)]), Look::Contents(vec![(Card::Icon(photo), &BACK)])];
        looks.extend((0..6).map(Look::Special));
        looks.push(Look::Both(3, vec![(Card::Page, &BACK)]));
        for style in [1, 2] {
            for look in &looks {
                let pm = render(look, style, 64).expect("rendered");
                let px = |x: u32, y: u32| pm.pixel(x, y).unwrap().alpha();
                assert!(px(40, 40) > 200, "the folder is drawn");
                assert_eq!(px(1, 1), 0, "the corner stays clear");
            }
        }
    }

    /// Every look saved as PNGs (256, 48, 16 px) to `$FOLDERS10_OUT`, to look at: `cargo test sheet -- --ignored`.
    #[test]
    #[ignore]
    fn sheet() {
        let out = std::path::PathBuf::from(std::env::var("FOLDERS10_OUT").expect("FOLDERS10_OUT"));
        let photo = Pixmap::decode_png(include_bytes!("../icons/sample-photo.png")).unwrap();
        let named = |style: u8| -> Vec<(String, Look)> {
            let mut v = vec![
                ("empty".to_string(), Look::Closed),
                ("one-picture".into(), Look::Contents(vec![(Card::Photo(photo.clone(), false), &BACK)])),
                ("two-pictures".into(), Look::Contents(vec![(Card::Photo(photo.clone(), false), &BACK), (Card::Photo(photo.clone(), false), &FRONT)])),
                ("text-file".into(), Look::Contents(vec![(Card::Page, &BACK)])),
                ("mixed".into(), Look::Contents(vec![(Card::Photo(photo.clone(), false), &BACK), (Card::Page, &FRONT)])),
                ("icon-file".into(), Look::Contents(vec![(Card::Icon(photo.clone()), &BACK)])),
                ("folders".into(), Look::Folders),
                ("video".into(), Look::Contents(vec![(Card::Photo(photo.clone(), true), &BACK)])),
            ];
            v.retain(|_| style == 1);
            let names = ["desktop", "downloads", "documents", "pictures", "music", "videos"];
            v.extend((0..6).map(|k| (format!("special-{}", names[k]), Look::Special(k))));
            v.extend([2, 3, 4, 5].map(|k| (format!("both-{}", names[k]), Look::Both(k, vec![(if k == 2 { Card::Page } else { Card::Photo(photo.clone(), k == 5) }, &BACK)]))));
            v
        };
        for style in [1u8, 2] {
            for (name, look) in named(style) {
                for size in [256, 48, 16] {
                    let pm = render(&look, style, size).unwrap();
                    pm.save_png(out.join(format!("s{style}-{name}-{size}.png"))).unwrap();
                }
            }
        }
    }
}
