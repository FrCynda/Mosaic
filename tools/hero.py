#!/usr/bin/env python3
"""docs/hero.svg: the README's animated tour of Mosaic, generated.

Pure vector, animated with CSS keyframes alone. GitHub shows README images through <img>, served from
raw.githubusercontent.com under `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline';
sandbox`: no script, no web font, no external or embedded bitmap. Inline <style> and keyframes are fine.
Colours, sizes and layout follow src/native*.rs (100% scale); text is measured with the Windows fonts
through Pillow, so it is rebuilt on Windows:  python tools/hero.py
"""
import os
import random
import sys
from PIL import ImageFont

T = 36.5            # the loop, seconds
STILL = 5.9         # the frame shown when motion is reduced: a video playing in the preview pane
CW, CH = 1200, 820  # the card
EASE = 'cubic-bezier(.45,0,.55,1)'  # ≈ smoothstep, as Mosaic's own fades
E = 0.004           # an instant change, seconds
RESET, DIP = 33.5, 0.3  # where the loop cross-fades back to its first frame (under the end card)
SANS = "'Segoe UI',system-ui,-apple-system,'Helvetica Neue',Arial,sans-serif"
MONO = "Consolas,'Cascadia Mono',ui-monospace,Menlo,monospace"

# -- themes (src/native.rs THEMES): bg card text muted accent line hl row sel
KEYS = 'bg card text muted accent line hl row sel'.split()
THEMES = {
    'Midnight': '0f1216 181c22 e6e8eb 8b93a0 7aa2ff 262b33 1f2a44 1c2129 243257',
    'Graphite': '121315 1b1c1f e7e8ea 8e9299 b4bccb 2b2d31 2a2c31 202124 373a41',
    'Navy': '0a0f20 111831 e4ebfa 8a97bd 5b95ff 1f2a4d 182550 141d3b 1f3575',
    'Ocean': '071518 0d2127 dcf3f5 7fa6ab 3fd9e0 18353d 10343c 102a30 114a54',
    'Forest': '09130f 0f1f18 dcf3e6 7fa592 3fe08a 1b3328 133325 12291f 164a31',
    'Sunset': '160b08 21110c fbe6da b08f80 ff9a4a 3a1f16 3d1e12 2b1610 5c2a14',
    'Rose': '140a11 20101b fbe0ee b08aa0 ff7ab8 3a1d31 3c1631 2a1323 5c1d47',
    'Amethyst': '0e0b1b 17122b ece4fa 9a8fbd b07bff 2b2148 2b1e52 1f1840 3c2878',
    'Daylight': 'ffffff ffffff 1a1d21 6b7280 317dcf d6dae1 e9f1fd f3f5f8 d7e6fb',
    'Fog': 'f4f5f7 fbfbfc 1b1d21 6a707a 4f5868 dcdfe4 e7e9ed eceef1 d8dce3',
    'Mint': 'f3faf6 ffffff 14231c 5f7a6c 14996a d3e8dc dff4ea e9f5ef c6ecda',
    'Lagoon': 'f2f9fa ffffff 0f2226 5b7a80 138a96 cfe5e8 dcf1f3 e8f4f5 c4e8ec',
    'Paper': 'f8f5ef fffdf9 2a251e 7d7366 b8640e e6dfd3 f4e8d4 f1ece3 efd9b8',
    'Sunrise': 'fff6f1 ffffff 2b1810 8a6a5c e0541c f0dbd0 fde7da faefe8 fad2bd',
    'Blossom': 'fdf4f8 ffffff 2a1020 8a6478 d6337f f0d9e4 fbe0ee f8edf2 f6cde0',
    'Lavender': 'f7f5fd ffffff 1d1630 6e6688 7a4ae6 e2ddf2 ece4fa f1eef9 dccff7',
}


class Pal:
    def __init__(self, name):
        self.name = name
        for k, v in zip(KEYS, THEMES[name].split()):
            setattr(self, k, '#' + v)


def mix(a, b, k):
    x, y = int(a[1:], 16), int(b[1:], 16)
    return '#' + ''.join(f'{round(((x >> s) & 255) * (1 - k) + ((y >> s) & 255) * k):02x}' for s in (16, 8, 0))


def ink(c):
    """Text on a fill of colour `c` (the Apply button's)."""
    v = int(c[1:], 16)
    lum = (0.2126 * (v >> 16) + 0.7152 * ((v >> 8) & 255) + 0.0722 * (v & 255)) / 255
    return '#0f1216' if lum > 0.5 else '#ffffff'


# -- text
FONTS = {}


def font(w=400, size=12, mono=False):
    k = (w, size, mono)
    if k not in FONTS:
        f = 'consola.ttf' if mono else {400: 'segoeui.ttf', 600: 'seguisb.ttf', 700: 'segoeuib.ttf'}[w]
        FONTS[k] = ImageFont.truetype(os.path.join(os.environ.get('WINDIR', r'C:\Windows'), 'Fonts', f), size)
    return FONTS[k]


def tw(s, size=12, w=400, mono=False):
    return font(w, size, mono).getlength(s)


def fit(s, width, size=12, w=400):
    if tw(s, size, w) <= width:
        return s
    while s and tw(s + '…', size, w) > width:
        s = s[:-1]
    return s.rstrip() + '…'


def num(v):
    s = f'{v:.2f}'.rstrip('0').rstrip('.')
    return '0' if s in ('', '-0') else s


def esc(s):
    return s.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')


def text(x, cy, s, fill, size=12, w=400, anchor=None, mono=False, op=None, raw=False, cls=None):
    """`s` centred on the line at `cy`, as DirectWrite sets it: Segoe UI's baseline 1.0791 em under the top
    of a round(1.4 em) line, Consolas' .92 em under the top of a 1.17 em one."""
    base = cy + .335 * size if mono else cy - round(size * 1.4) / 2 + size * 1.0791
    a = f'x="{num(x)}" y="{num(base)}" fill="{fill}"'
    if size != 12:
        a += f' font-size="{num(size)}"'
    if w != 400:
        a += f' font-weight="{w}"'
    if anchor:
        a += f' text-anchor="{anchor}"'
    if mono or cls:
        a += f' class="{" ".join(c for c in ("m" if mono else "", cls or "") if c)}"'
    if mono:
        a += ' xml:space="preserve"'
    if op is not None:
        a += f' opacity="{num(op)}"'
    return f'<text {a}>{s if raw else esc(s)}</text>'


def rect(x, y, w, h, fill=None, rx=0, stroke=None, op=None, sw=1):
    if stroke:  # a crisp line: on the half pixel, inside the box
        x, y, w, h = x + sw / 2, y + sw / 2, w - sw, h - sw
        rx = max(0, rx - sw / 2)
    a = f'x="{num(x)}" y="{num(y)}" width="{num(w)}" height="{num(h)}"'
    if rx:
        a += f' rx="{num(rx)}"'
    a += f' fill="{fill or "none"}"'
    if stroke:
        a += f' stroke="{stroke}"' + (f' stroke-width="{num(sw)}"' if sw != 1 else '')
    if op is not None:
        a += f' opacity="{num(op)}"'
    return f'<rect {a}/>'


def hline(x0, x1, y, c):
    return f'<path d="M{num(x0)} {num(y + .5)}H{num(x1)}" stroke="{c}"/>'


def vline(x, y0, y1, c):
    return f'<path d="M{num(x + .5)} {num(y0)}V{num(y1)}" stroke="{c}"/>'


def use(ref, x, y, w, h=None):
    return f'<use href="#{ref}" x="{num(x)}" y="{num(y)}" width="{num(w)}" height="{num(h or w)}"/>'


def g(inner, tr=None, cls=None, extra=''):
    a = (f' transform="{tr}"' if tr else '') + (f' class="{cls}"' if cls else '') + extra
    return f'<g{a}>{inner}</g>'


# -- animation: every element runs one T-second keyframe track from the same start, so they stay in step
def pct(t):
    return f'{100 * t / T:.3f}'.rstrip('0').rstrip('.') + '%'


def decl(fmt, v):
    vs = v if isinstance(v, tuple) else (v,)
    return fmt.format(*[num(x) if isinstance(x, (int, float)) else x for x in vs])


class Anim:
    def __init__(self):
        self.rules, self.memo = [], {}

    def track(self, fmt, pts, ease=EASE):
        """A class animating `fmt` (a CSS declaration with {} for the values) through pts [(t, value)], in
        time order. The loop's ends meet: what holds at T is what held at 0."""
        pts = sorted(pts, key=lambda p: p[0])
        if pts[0][0] > 0:
            pts = [(0.0, pts[0][1])] + pts
        if pts[-1][0] < T:
            pts = pts + [(T, pts[0][1])]
        key = (fmt, tuple(pts), ease)
        if key in self.memo:
            return self.memo[key]
        name = f'a{len(self.memo):x}'
        self.memo[key] = name
        by = {}
        for t, v in pts:
            by.setdefault(v, {})[pct(t)] = 1
        frames = ''.join(f'{",".join(p)}{{{decl(fmt, v)}}}' for v, p in by.items())
        still = [v for t, v in pts if t <= STILL][-1]
        self.rules.append(f'.{name}{{{decl(fmt, still)};animation:{name} {num(T)}s {ease} infinite}}@keyframes {name}{{{frames}}}')
        return name

    def vis(self, spans):
        """Shown during spans [(from, to, fade_in, fade_out)]. Gone at once (fade_out 0), it stays whole until
        what replaces it is (E later), then goes: else, for the frame where both are half there, all that
        they share dims, and typing (a look a key) flickers."""
        on = any(a <= 0 < b for a, b, _, _ in spans)
        pts = [(0.0, int(on))]
        for a, b, fi, fo in sorted(spans):
            if a > 0:
                pts += [(a, 0), (min(a + max(fi, E), b), 1)]
            if b < T:
                pts += [(b, 1), (min(b + fo, T), 0)] if fo else [(min(b + E, T), 1), (min(b + 2 * E, T), 0)]
        if all(v for _, v in pts):
            return None
        return self.track('opacity:{};visibility:{}', [(t, (v, 'visible' if v else 'hidden')) for t, v in pts])

    def show(self, inner, spans):
        c = self.vis(spans)
        return g(inner, cls=c) if c else inner


A = Anim()


def timeline(snaps, render, fade=(0, 0), fx=None):
    """One element per distinct look, shown while the snapshots have it. snaps [(t, state)]; render(state) is
    the markup; a change fades the old look out and the new one in over `fade` (fx {t: fade} for some)."""
    fx = fx or {}
    looks, cur = {}, None
    for t, st in snaps:
        m = render(st)
        if m == cur:
            continue
        fo, fi = (DIP, DIP) if t == RESET else fx.get(round(t, 3), fade)
        if cur:
            if looks[cur][-1][0] >= t:  # replaced at once: never shown
                looks[cur].pop()
            else:
                looks[cur][-1][1], looks[cur][-1][3] = t, fo
        if m:
            looks.setdefault(m, []).append([t, T, fi, 0])
        cur = m
    return ''.join(A.show(m, [tuple(s) for s in sp]) for m, sp in looks.items() if sp)


def stepped(pts, render):
    """Values that change at once: pts [(t, v)] from t = 0."""
    out = {}
    for k, (t, v) in enumerate(pts):
        out.setdefault(v, []).append((t, pts[k + 1][0] if k + 1 < len(pts) else T, 0, 0))
    return ''.join(A.show(render(v), sp) for v, sp in out.items())


def about(cx, cy, cls, inner):
    """`inner` transformed by `cls` about (cx, cy): CSS transforms on SVG turn about the local origin."""
    return f'<g transform="translate({num(cx)} {num(cy)})"><g class="{cls}"><g transform="translate({num(-cx)} {num(-cy)})">{inner}</g></g></g>'


# -- shapes
def shadow(x, y, w, h, r=34, op=.5, dy=14):
    """A soft drop shadow without a filter (an animated image re-runs filters on every frame): nine
    gradient-filled slices."""
    y += dy
    s = [rect(x, y, w, h, '#000', op=op)]
    for sx, sy, sw, sh, gid in [
            (x - r, y - r, r, r, 'sh-tl'), (x + w, y - r, r, r, 'sh-tr'), (x - r, y + h, r, r, 'sh-bl'), (x + w, y + h, r, r, 'sh-br'),
            (x, y - r, w, r, 'sh-t'), (x, y + h, w, r, 'sh-b'), (x - r, y, r, h, 'sh-l'), (x + w, y, r, h, 'sh-r')]:
        s.append(f'<rect x="{num(sx)}" y="{num(sy)}" width="{num(sw)}" height="{num(sh)}" fill="url(#{gid})" opacity="{num(op)}"/>')
    return ''.join(s)


def shadow_defs():
    stops = '<stop offset="0"/><stop offset=".3" stop-opacity=".5"/><stop offset=".65" stop-opacity=".14"/><stop offset="1" stop-opacity="0"/>'
    out = [f'<radialGradient id="{gid}" cx="{cx}" cy="{cy}" r="1">{stops}</radialGradient>'
           for gid, cx, cy in [('sh-tl', 1, 1), ('sh-tr', 0, 1), ('sh-bl', 1, 0), ('sh-br', 0, 0)]]
    out += [f'<linearGradient id="{gid}" x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}">{stops}</linearGradient>'
            for gid, x1, y1, x2, y2 in [('sh-t', 0, 1, 0, 0), ('sh-b', 0, 0, 0, 1), ('sh-l', 1, 0, 0, 0), ('sh-r', 0, 0, 1, 0)]]
    return ''.join(out)


def tab_path(x0, x1, top, bottom, r=8, f=8):
    """Canvas::tab: rounded on top, flaring out into what's under it."""
    return (f'M{num(x0 - f)} {num(bottom)}Q{num(x0)} {num(bottom)} {num(x0)} {num(bottom - f)}V{num(top + r)}'
            f'Q{num(x0)} {num(top)} {num(x0 + r)} {num(top)}H{num(x1 - r)}Q{num(x1)} {num(top)} {num(x1)} {num(top + r)}'
            f'V{num(bottom - f)}Q{num(x1)} {num(bottom)} {num(x1 + f)} {num(bottom)}')


def chevron_down(cx, cy, c, s=4):
    return f'<path d="M{num(cx - s)} {num(cy - s / 2)}l{num(s)} {num(s)} {num(s)} {num(-s)}" fill="none" stroke="{c}" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/>'


def chevron_right(cx, cy, c, s=3.5):
    return f'<path d="M{num(cx - s / 2)} {num(cy - s)}l{num(s)} {num(s)} {num(-s)} {num(s)}" fill="none" stroke="{c}" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/>'


def tri(cx, cy, c, up=True, s=3.5):
    d = (f'M{num(cx - s)} {num(cy + s * .6)}h{num(2 * s)}l{num(-s)} {num(-s * 1.2)}z' if up else
         f'M{num(cx - s)} {num(cy - s * .6)}h{num(2 * s)}l{num(-s)} {num(s * 1.2)}z')
    return f'<path d="{d}" fill="{c}"/>'


# -- icons: Windows 11 look-alikes, 16 units
DOC = ('<path d="M3.6 1.5h5.7L13 5.2v9.1a.7.7 0 0 1-.7.7H3.6a.7.7 0 0 1-.7-.7V2.2a.7.7 0 0 1 .7-.7z" fill="#fbfbfb" stroke="#a3a9b1" stroke-width=".6"/>'
       '<path d="M9.3 1.5v3a.7.7 0 0 0 .7.7h3" fill="#e4e6e9" stroke="#a3a9b1" stroke-width=".6"/>')
FOLDER = ('<path d="M1.5 3.2c0-.66.54-1.2 1.2-1.2h3.4c.32 0 .62.13.85.35L8.2 3.6h5.1c.66 0 1.2.54 1.2 1.2V6H1.5z" fill="#e8a317"/>'
          '<path d="M1.5 5.6c0-.39.31-.7.7-.7h11.6c.39 0 .7.31.7.7v6.7c0 .66-.54 1.2-1.2 1.2H2.7c-.66 0-1.2-.54-1.2-1.2z" fill="url(#g-fold)"/>')
PHOTO = ('<path d="M1.3 11.6l3.6-3.8 2.6 2.6 2.1-2 5.1 4.3v.8a2.2 2.2 0 0 1-2.2 2.2H3.5a2.2 2.2 0 0 1-2.2-2.2z" fill="#fff" opacity=".92"/>')
NOTE = '<path d="M10.6 4.1v5.6a1.6 1.6 0 1 1-1-1.48V6l-3 .8v4.4a1.6 1.6 0 1 1-1-1.48V5.4z" fill="#fff"/>'


def badge(color, glyph):
    return f'<rect x=".8" y="6.6" width="8.4" height="8.4" rx="1.4" fill="{color}"/>{glyph}'


ICONS = {
    'folder': FOLDER,
    'xlsx': DOC + '<path d="M9.8 7.3h2.4M9.8 9.6h2.4M9.8 11.9h2.4" stroke="#21a366" stroke-width=".9"/>'
            + badge('#107c41', '<path d="M3.1 8.9l3.8 3.8M6.9 8.9l-3.8 3.8" stroke="#fff" stroke-width="1.3" stroke-linecap="round"/>'),
    'docx': DOC + '<path d="M9.8 7.3h2.4M9.8 9.6h2.4M9.8 11.9h2.4" stroke="#2b7cd3" stroke-width=".9"/>'
            + badge('#185abd', '<path d="M2.6 8.9l1.1 3.9 1.3-3 1.3 3 1.1-3.9" fill="none" stroke="#fff" stroke-width="1" stroke-linejoin="round" stroke-linecap="round"/>'),
    'pptx': DOC + '<path d="M9.8 9.6h2.4M9.8 11.9h2.4" stroke="#d35230" stroke-width=".9"/>'
            + badge('#c43e1c', '<path d="M4 12.9V8.8h1.6a1.25 1.25 0 0 1 0 2.5H4" fill="none" stroke="#fff" stroke-width="1.1" stroke-linejoin="round"/>'),
    'pdf': DOC + badge('#d93025', '<path d="M2.7 12.6c1.3-.5 2.6-2.4 2.8-3.5.1-.8-.7-.9-.9-.2-.3 1.1 1 2.9 2.6 3.3" fill="none" stroke="#fff" stroke-width=".9" stroke-linecap="round"/>'),
    'txt': DOC + '<path d="M5 6.6h6M5 8.6h6M5 10.6h6M5 12.6h3.8" stroke="#8a9099" stroke-width=".8"/>',
    'md': DOC + '<path d="M5 8.6h6M5 10.6h6M5 12.6h3.8" stroke="#8a9099" stroke-width=".8"/><path d="M5 6.6h3" stroke="#6e56cf" stroke-width="1.1"/>',
    'csv': DOC + '<path d="M4.6 7.2h6.8M4.6 9.6h6.8M4.6 12h6.8M7 6v7.2M9.3 6v7.2" stroke="#21a366" stroke-width=".7"/>',
    'py': '<path d="M7.9 1.4c-3 0-2.8 1.3-2.8 1.3v1.4H8v.4H4S2.1 4.3 2.1 7.4s1.7 3 1.7 3h1V9s-.1-1.7 1.7-1.7h2.8s1.6 0 1.6-1.6V3.1s.2-1.7-3-1.7zM6.3 2.3a.5.5 0 1 1 0 1 .5.5 0 0 1 0-1z" fill="#3776ab"/>'
          '<path d="M8.1 14.6c3 0 2.8-1.3 2.8-1.3v-1.4H8v-.4h4s1.9.2 1.9-2.9-1.7-3-1.7-3h-1V7s.1 1.7-1.7 1.7H6.7s-1.6 0-1.6 1.6v2.6s-.2 1.7 3 1.7zm1.6-.9a.5.5 0 1 1 0-1 .5.5 0 0 1 0 1z" fill="#ffd43b"/>',
    'mp4': '<rect x="1.3" y="2.8" width="13.4" height="10.4" rx="2.2" fill="url(#g-vid)"/><path d="M6.6 5.7v4.6l3.9-2.3z" fill="#fff"/>',
    'mp3': f'<rect x="1.3" y="1.3" width="13.4" height="13.4" rx="3" fill="url(#g-aud)"/>{NOTE}',
    'img': f'<rect x="1.3" y="2.3" width="13.4" height="11.4" rx="2.2" fill="url(#g-img)"/>{PHOTO}<circle cx="11" cy="5.6" r="1.3" fill="#fff6c2"/>',
    'zip': FOLDER + '<path d="M8 4.9v8.6" stroke="#7a5410" stroke-width="1.6" stroke-dasharray="1 1"/>',
    'msi': '<path d="M2 5l6-3 6 3v6.5l-6 3-6-3z" fill="#b7895a"/><path d="M2 5l6 3 6-3M8 8v6.5" fill="none" stroke="#8a6238" stroke-width=".8"/><path d="M2 5l6-3 6 3-6 3z" fill="#d7aa79"/>',
    # places
    'home': '<path d="M8 1.6 1.4 7.2c-.3.3-.1.8.3.8h1.2v5.8c0 .4.3.7.7.7h3.2V10h2.4v4.5h3.2c.4 0 .7-.3.7-.7V8h1.2c.4 0 .6-.5.3-.8z" fill="url(#g-home)"/>',
    'gallery': '<rect x="1.3" y="2.3" width="13.4" height="11.4" rx="2.2" fill="#1f86e6"/><rect x="2.8" y="3.8" width="10.4" height="8.4" rx="1" fill="#8fd0ff"/><path d="M2.8 11.2l3-3.1 2.2 2.1 1.8-1.7 3.4 2.9v.8H2.8z" fill="#1462b8"/><circle cx="10.4" cy="6" r="1.1" fill="#fff"/>',
    'desktop': '<rect x="1.3" y="2.3" width="13.4" height="9.4" rx="1.6" fill="url(#g-desk)"/><rect x="6.2" y="11.7" width="3.6" height="1.6" fill="#7d8796"/><rect x="4.4" y="13.2" width="7.2" height="1.3" rx=".65" fill="#9aa3b1"/>',
    'downloads': '<rect x="1.3" y="1.3" width="13.4" height="13.4" rx="3" fill="url(#g-down)"/><path d="M8 4.1v6M5.4 7.7 8 10.3l2.6-2.6M5 12.4h6" fill="none" stroke="#fff" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/>',
    'documents': '<path d="M3.4 1.3h6.2l3.9 3.9v9a.8.8 0 0 1-.8.8H3.4a.8.8 0 0 1-.8-.8V2.1a.8.8 0 0 1 .8-.8z" fill="url(#g-docs)"/><path d="M9.6 1.3v3.3a.6.6 0 0 0 .6.6h3.3" fill="#c6d3e6"/><path d="M5 8h6M5 10h6M5 12h4" stroke="#fff" stroke-width=".9"/>',
    'pictures': f'<rect x="1.3" y="2.3" width="13.4" height="11.4" rx="2.2" fill="url(#g-pics)"/>{PHOTO}<circle cx="11" cy="5.6" r="1.3" fill="#fff"/>',
    'music': f'<rect x="1.3" y="1.3" width="13.4" height="13.4" rx="3" fill="url(#g-aud)"/>{NOTE}',
    'videos': '<rect x="1.3" y="2.8" width="13.4" height="10.4" rx="2.2" fill="url(#g-vid)"/><path d="M6.6 5.7v4.6l3.9-2.3z" fill="#fff"/>',
    'pc': '<rect x="1.3" y="2" width="13.4" height="9.4" rx="1.4" fill="#1c6fd1"/><rect x="2.3" y="3" width="11.4" height="7.4" rx=".6" fill="url(#g-desk)"/><rect x="6.2" y="11.4" width="3.6" height="1.8" fill="#7d8796"/><rect x="4.4" y="13.1" width="7.2" height="1.3" rx=".65" fill="#9aa3b1"/>',
    'network': '<circle cx="8" cy="8" r="6.5" fill="url(#g-desk)"/><path d="M1.5 8h13M8 1.5c-2.4 2.6-2.4 10.4 0 13M8 1.5c2.4 2.6 2.4 10.4 0 13M2.6 4.6h10.8M2.6 11.4h10.8" fill="none" stroke="#dff1ff" stroke-width=".7"/>',
    'linux': '<ellipse cx="8" cy="8.6" rx="4.7" ry="6.1" fill="#202124"/><ellipse cx="8" cy="10.2" rx="3" ry="4" fill="#f4f4f4"/><circle cx="6.7" cy="5.2" r=".9" fill="#fff"/><circle cx="9.3" cy="5.2" r=".9" fill="#fff"/><circle cx="6.9" cy="5.3" r=".4" fill="#111"/><circle cx="9.1" cy="5.3" r=".4" fill="#111"/><path d="M6.8 6.6h2.4L8 7.9z" fill="#f6b300"/><ellipse cx="5.4" cy="14.4" rx="2" ry="1" fill="#f6b300"/><ellipse cx="10.6" cy="14.4" rx="2" ry="1" fill="#f6b300"/>',
    'ps': '<path d="M3.6 2.5h11.2c.4 0 .6.3.5.7l-2.5 9.6c-.1.4-.5.7-.9.7H.7c-.4 0-.6-.3-.5-.7l2.5-9.6c.1-.4.5-.7.9-.7z" fill="url(#g-ps)"/><path d="M4.4 5.4 7.2 8 3.6 10.6M7.4 10.6h3" fill="none" stroke="#fff" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/>',
    'cmd': '<rect x="1.3" y="2.3" width="13.4" height="11.4" rx="1.6" fill="#1b1b1d" stroke="#71757c" stroke-width=".6"/><path d="M1.6 4.8h12.8" stroke="#3b3d42"/><path d="M3.8 7.2l2 1.6-2 1.6M7 11h3.2" fill="none" stroke="#e8e8e8" stroke-linecap="round" stroke-linejoin="round"/>',
}
ICON_OF = {'folder': 'folder', 'xlsx': 'xlsx', 'docx': 'docx', 'pptx': 'pptx', 'pdf': 'pdf', 'txt': 'txt', 'md': 'md', 'csv': 'csv',
           'py': 'py', 'mp4': 'mp4', 'mp3': 'mp3', 'flac': 'mp3', 'jpg': 'img', 'png': 'img', 'zip': 'zip', 'msi': 'msi', 'exe': 'logo'}
TYPE_OF = {'folder': 'File folder', 'xlsx': 'Microsoft Excel Worksheet', 'docx': 'Microsoft Word Document', 'pptx': 'Microsoft PowerPoint Presentation',
           'pdf': 'PDF Document', 'txt': 'Text Document', 'md': 'MD File', 'csv': 'CSV File', 'py': 'Python File', 'mp4': 'MP4 Video',
           'mp3': 'MP3 File', 'flac': 'FLAC File', 'jpg': 'JPG File', 'png': 'PNG File', 'zip': 'Compressed (zipped) Folder',
           'msi': 'Windows Installer Package', 'exe': 'Application'}

# Mosaic's own mark, from the old docs/banner.svg (1024 units; its drop-shadow filter becomes an offset copy)
CARD = 'M452 452Q452 436 466 428L612 345Q624 336 640 336H848Q870 336 870 358V772Q870 797 845 797H478Q452 797 452 772Z'
LOGO = ('<g transform="translate(512 512) scale(1.4) translate(-544 -509.5)">'
        '<path d="M490 262Q500 257 520 257H780Q808 257 808 285V335H490Z" fill="url(#mi-sheet)" opacity=".9"/>'
        '<path d="M270 222H430Q452 222 466 238L500 276Q512 293 534 293H745Q778 293 778 326V772H270Q218 772 218 720V274Q218 222 270 222Z" fill="url(#mi-folder)"/>'
        f'<path d="{CARD}" fill="#000" opacity=".25" transform="translate(0 10)"/><path d="{CARD}" fill="#fff"/>'
        '<g stroke-linejoin="round" stroke-width="16"><polygon points="480,458 636,374 638,568 480,616" fill="url(#mi-p1)" stroke="url(#mi-p1)"/>'
        '<polygon points="675,374 830,374 830,516 675,562" fill="url(#mi-p2)" stroke="url(#mi-p2)"/>'
        '<polygon points="480,660 638,613 638,760 480,760" fill="url(#mi-p3)" stroke="url(#mi-p3)"/>'
        '<polygon points="675,603 830,556 830,760 675,760" fill="url(#mi-p4)" stroke="url(#mi-p4)"/></g></g>')


def stops(st):
    return ''.join(f'<stop offset="{num(s[0])}" stop-color="{s[1]}"' + (f' stop-opacity="{num(s[2])}"' if len(s) > 2 else '') + '/>' for s in st)


def lg(gid, st, x1=0, y1=0, x2=0, y2=1, extra=''):
    return f'<linearGradient id="{gid}" x1="{num(x1)}" y1="{num(y1)}" x2="{num(x2)}" y2="{num(y2)}"{extra}>{stops(st)}</linearGradient>'


def rg(gid, st, cx=.5, cy=.5, r=.5, extra=''):
    return f'<radialGradient id="{gid}" cx="{num(cx)}" cy="{num(cy)}" r="{num(r)}"{extra}>{stops(st)}</radialGradient>'


def gradients():
    return ''.join([
        lg('g-fold', [(0, '#ffd75a'), (1, '#f6b52c')]), lg('g-vid', [(0, '#9b7bff'), (1, '#5a45e8')], 0, 0, 1, 1),
        lg('g-aud', [(0, '#ff8a4c'), (1, '#f0436a')], 0, 0, 1, 1), lg('g-img', [(0, '#5fd0ff'), (1, '#1677e8')], 0, 0, 1, 1),
        lg('g-pics', [(0, '#4fb6ff'), (1, '#1a6fe0')], 0, 0, 1, 1), lg('g-home', [(0, '#ffb35c'), (1, '#f2711c')]),
        lg('g-desk', [(0, '#49b4ff'), (1, '#1a73e8')]), lg('g-down', [(0, '#22c3a5'), (1, '#0b8f7c')], 0, 0, 1, 1),
        lg('g-docs', [(0, '#93a8c6'), (1, '#6c84a8')]), lg('g-ps', [(0, '#3f86f0'), (1, '#1f4fb5')], 0, 0, 1, 1),
        lg('g-drive', [(0, '#d5dae1'), (1, '#8e97a4')]),
        lg('mi-folder', [(0, '#2fa2ff'), (.55, '#2171e6'), (1, '#1443b0')], 0, 0, .35, 1), lg('mi-sheet', [(0, '#9bb3de'), (1, '#e9f0fc')], 0, 0, 1, 1),
        lg('mi-p1', [(0, '#5fe3c4'), (1, '#1ea0b2')], 0, 0, .4, 1), lg('mi-p2', [(0, '#ffe94f'), (1, '#ff8620')], .2, 0, .8, 1),
        lg('mi-p3', [(0, '#a070ff'), (1, '#5646ff')], .2, 0, .7, 1), lg('mi-p4', [(0, '#45b4ff'), (1, '#1368f5')], .2, 0, .7, 1),
    ])


def icon_defs():
    out = [f'<symbol id="i-{k}" viewBox="0 0 16 16">{v}</symbol>' for k, v in ICONS.items()]
    out.append(f'<symbol id="i-logo" viewBox="0 0 1024 1024">{LOGO}</symbol>')
    out.append('<symbol id="i-drive" viewBox="0 0 32 32"><rect x="5" y="2" width="5.6" height="5.6" fill="#0a84ff"/><rect x="11.4" y="2" width="5.6" height="5.6" fill="#0a84ff"/>'
               '<rect x="5" y="8.4" width="5.6" height="5.6" fill="#0a84ff"/><rect x="11.4" y="8.4" width="5.6" height="5.6" fill="#0a84ff"/>'
               '<rect x="1.5" y="16" width="29" height="12" rx="2.6" fill="url(#g-drive)"/><rect x="1.5" y="16" width="29" height="3" rx="1.5" fill="#fff" opacity=".35"/>'
               '<path d="M5 24h14" stroke="#5f6772" stroke-width="1.2" stroke-linecap="round"/><circle cx="26" cy="23.5" r="1.3" fill="#3fe07a"/></symbol>')
    return ''.join(out)


# -- the made-up photos and videos, drawn
def scenes():
    rnd = random.Random(7)
    city, x = [], 0
    while x < 150:
        bw, bh = rnd.randint(9, 17), rnd.randint(22, 62)
        city.append(f'<rect x="{x}" y="{100 - bh}" width="{bw}" height="{bh}" fill="#0c0a22"/>')
        for wy in range(100 - bh + 4, 96, 6):
            for wx in range(x + 2, x + bw - 2, 4):
                if rnd.random() < .33:
                    city.append(f'<rect x="{wx}" y="{wy}" width="1.6" height="2.2" fill="#ffd27a" opacity="{num(rnd.uniform(.5, 1))}"/>')
        x += bw + rnd.randint(0, 2)
    stars = ''.join(f'<circle cx="{rnd.randint(0, 150)}" cy="{rnd.randint(0, 45)}" r="{num(rnd.uniform(.3, .8))}" fill="#fff" opacity="{num(rnd.uniform(.4, .9))}"/>' for _ in range(28))
    pines = []
    for _ in range(16):
        px, s = rnd.uniform(-5, 165), rnd.uniform(.7, 1.3)
        if 58 < px < 102:
            continue
        top = 92 - 70 * s
        pines.append(f'<path d="M{num(px)} {num(top)}l{num(10 * s)} {num(30 * s)}h{num(-5 * s)}l{num(9 * s)} {num(26 * s)}h{num(-28 * s)}l{num(9 * s)} {num(-26 * s)}h{num(-5 * s)}z" fill="{rnd.choice(["#163b26", "#1d4a2f", "#12301f"])}"/>')
    motes = ''.join(f'<circle cx="{num(rnd.uniform(62, 146))}" cy="{num(rnd.uniform(6, 58))}" r="{num(rnd.uniform(.3, .7))}" fill="#ffe7a3" opacity="{num(rnd.uniform(.4, .9))}"/>' for _ in range(24))
    # a blocky world, 5 units a block: hills of grass on dirt on stone, a pond, two oaks and a little house
    heights = [9, 9, 10, 10, 11, 11, 11, 10, 10, 9, 8, 7, 7, 7, 7, 8, 8, 9, 9, 10, 10, 10, 10, 10, 10, 10, 11, 11, 10, 10]
    blocks = ['<rect width="150" height="100" fill="url(#sc-mc-sky)"/><rect x="106" y="8" width="16" height="16" fill="#fff8d6" opacity=".35"/><rect x="110" y="12" width="8" height="8" fill="#fffbe8"/>']
    for x, y, n in [(10, 18, 6), (50, 11, 4), (78, 24, 5)]:
        blocks.append(f'<rect x="{x}" y="{y}" width="{n * 5}" height="4" fill="#fff" opacity=".9"/><rect x="{x + 5}" y="{y - 3}" width="{(n - 2) * 5}" height="3" fill="#fff" opacity=".9"/>')
    blocks.append('<rect x="50" y="60" width="30" height="10" fill="#3d6ee0" opacity=".85"/><rect x="50" y="60" width="30" height="1" fill="#8fb0ff"/>')
    for i, hb in enumerate(heights):
        x, top = i * 5, 100 - hb * 5
        blocks.append(f'<rect x="{x}" y="{top}" width="5" height="5" fill="#5f9f35"/><rect x="{x}" y="{top}" width="5" height="1.5" fill="#7cc24a"/>'
                      f'<rect x="{x}" y="{top + 5}" width="5" height="10" fill="#866043"/><rect x="{x}" y="{top + 15}" width="5" height="{hb * 5 - 15}" fill="#7a7a7a"/>')
        for _ in range(3):
            blocks.append(f'<rect x="{x + rnd.randint(0, 3)}" y="{rnd.randint(top + 6, 98)}" width="1.5" height="1.5" fill="{rnd.choice(["#5d4330", "#5f5f5f", "#9a9a9a"])}"/>')
    for tx, ground in [(20, 45), (140, 50)]:  # oaks: a trunk three blocks high, leaves around its top
        blocks.append(f'<rect x="{tx}" y="{ground - 15}" width="5" height="15" fill="#6a4d2b"/><rect x="{tx - 10}" y="{ground - 25}" width="25" height="10" fill="#3f8f2b"/>'
                      f'<rect x="{tx - 5}" y="{ground - 30}" width="15" height="5" fill="#3f8f2b"/><rect x="{tx - 5}" y="{ground - 22}" width="5" height="5" fill="#2f6f20"/>'
                      f'<rect x="{tx + 5}" y="{ground - 27}" width="5" height="5" fill="#4ea336"/>')
    blocks.append('<rect x="100" y="35" width="25" height="15" fill="#b5884e"/><path d="M100 40h25M100 45h25" stroke="#8d6633" stroke-width=".6"/>'
                  '<rect x="105" y="40" width="5" height="10" fill="#7a5530"/><rect x="115" y="39" width="5" height="5" fill="#a9d8ff"/>'
                  '<rect x="96" y="30" width="33" height="5" fill="#7d5229"/><rect x="101" y="25" width="23" height="5" fill="#7d5229"/><rect x="106" y="20" width="13" height="5" fill="#7d5229"/>')
    defs = ''.join([
        lg('sc-alps', [(0, '#1d2a5c'), (.55, '#7b6bb0'), (1, '#f6a97c')]), rg('sc-sun', [(0, '#fff4cf'), (.35, '#ffd29a', .8), (1, '#ffb27a', 0)]),
        lg('sc-aur-sky', [(0, '#020b18'), (1, '#0b2a3b')]), lg('sc-aur', [(0, '#3dffb0', 0), (.5, '#3dffb0', .85), (1, '#7a5cff', 0)], 0, 0, 1, 0),
        lg('sc-erd-sky', [(0, '#151026'), (.5, '#4b2f4f'), (.85, '#c98a4b'), (1, '#e9b866')]), rg('sc-erd-glow', [(0, '#ffe9a8', .75), (.35, '#f2b84b', .35), (1, '#f2b84b', 0)]),
        lg('sc-mc-sky', [(0, '#6fb2f2'), (1, '#cfe9ff')]),
        lg('sc-lake-sky', [(0, '#8cc8f5'), (1, '#e3f2ff')]), lg('sc-water', [(0, '#79b2de'), (1, '#24527f')]),
        lg('sc-city', [(0, '#100a36'), (.6, '#3b1f6e'), (1, '#ff7393')]), lg('sc-tul-sky', [(0, '#a9d8ff'), (1, '#eef8ff')]),
        lg('sc-sea', [(0, '#16c3d1'), (1, '#0a5f93')]), lg('sc-sand', [(0, '#f6e2b6'), (1, '#e3c48c')]),
        lg('sc-forest', [(0, '#0d2a1c'), (.6, '#2d6a3e'), (1, '#173d27')]), lg('sc-trail', [(0, '#e4cf9c'), (1, '#9c8256')]),
        lg('sc-cover', [(0, '#2a1d0a'), (.55, '#6b4a12'), (1, '#e0b04a')], 0, 0, 1, 1), lg('sc-ray', [(0, '#fff8d0', .55), (1, '#fff8d0', 0)]),
    ])
    s = {
        'alps': ('0 0 150 100', '<rect width="150" height="100" fill="url(#sc-alps)"/><circle cx="104" cy="64" r="26" fill="url(#sc-sun)"/>'
                 '<path d="M0 70 18 52l12 8 18-22 14 17 16-13 17 16 17-18 18 16 20-10v54H0z" fill="#8a80c0" opacity=".95"/>'
                 '<path d="M48 38l-6 8 6-2 4 3zM112 40l-5 7 5-1 4 3zM78 42l-4 6 4-1 3 2z" fill="#fff" opacity=".9"/>'
                 '<path d="M0 82 22 66l18 10 20-16 22 18 22-14 22 16 24-10v30H0z" fill="#2d2f5c"/><path d="M0 92q40-8 75-2t75-2v12H0z" fill="#191b37"/>'),
        'aurora': ('0 0 150 100', f'<rect width="150" height="100" fill="url(#sc-aur-sky)"/>{stars}'
                   '<path d="M-5 42C30 18 60 56 95 30s45-6 60 4v16c-35-12-55 18-95 4S10 62-5 56z" fill="url(#sc-aur)"/>'
                   '<path d="M-5 30C40 10 70 40 110 20s40 2 45 6v8c-30-8-50 14-90 4S5 44-5 40z" fill="url(#sc-aur)" opacity=".45"/>'
                   '<path d="M0 78 20 62l14 9 22-20 18 17 20-14 22 18 18-10 16 8v30H0z" fill="#06131d"/><path d="M56 51l-6 7 6-2 4 3z" fill="#cfe3f2" opacity=".7"/>'),
        'erdtree': ('0 0 160 90', '<rect width="160" height="90" fill="url(#sc-erd-sky)"/><circle cx="104" cy="28" r="60" fill="url(#sc-erd-glow)"/>'
                    '<path d="M101.5 72C102.5 58 101 48 103 38h2.2c1.6 10 .4 22 1.8 34z" fill="#ffd66b"/><path d="M103.2 70c.6-10-.4-20 .9-32" stroke="#fff4cc" stroke-width=".5" fill="none"/>'
                    '<path d="M104 40Q100 30 87 30M104.5 38Q109 29 121 29M104 37Q103 25 104 15" fill="none" stroke="#ffe08a" opacity=".85"/>'
                    '<g fill="#ffd36b"><ellipse cx="104" cy="26" rx="34" ry="11" opacity=".45"/><ellipse cx="89" cy="31" rx="18" ry="6" opacity=".5"/>'
                    '<ellipse cx="120" cy="30" rx="19" ry="6" opacity=".5"/><ellipse cx="104" cy="16" rx="20" ry="8" opacity=".55"/></g>'
                    f'<ellipse cx="104" cy="23" rx="12" ry="6" fill="#fff1c2" opacity=".6"/>{motes}'
                    '<path d="M0 66q20-6 40-3t40-2 40 3 40-4v30H0z" fill="#3a2c45"/>'
                    '<g fill="#261d30"><rect x="14" y="57" width="26" height="9"/><rect x="14" y="51" width="4" height="7"/><rect x="24" y="46" width="4" height="12"/>'
                    '<path d="M23.5 46l2.5-5 2.5 5z"/><rect x="34" y="52" width="4" height="6"/></g>'
                    '<path d="M0 74q30-7 70-2t90-1v19H0z" fill="#1d1724"/><path d="M0 84q50-5 160 0v6H0z" fill="#120e17"/>'),
        'blocks': ('0 0 150 100', f'<g shape-rendering="crispEdges">{"".join(blocks)}</g>'),
        'lake': ('0 0 150 100', '<rect width="150" height="56" fill="url(#sc-lake-sky)"/><path d="M0 50 25 30l20 14 25-22 25 24 25-18 30 20v8H0z" fill="#53718f"/>'
                 '<path d="M70 22l-6 7 6-2 5 3zM25 30l-5 6 5-2 4 2zM120 28l-5 6 5-2 4 2z" fill="#fff" opacity=".9"/>'
                 '<rect y="50" width="150" height="7" fill="#21442f"/><rect y="57" width="150" height="43" fill="url(#sc-water)"/>'
                 '<path d="M0 64 25 84l20-14 25 22 25-24 25 18 30-20v-6H0z" fill="#53718f" opacity=".35"/><path d="M10 70h30M60 78h40M100 66h30" stroke="#fff" opacity=".25"/>'),
        'city': ('0 0 150 100', f'<rect width="150" height="100" fill="url(#sc-city)"/><circle cx="118" cy="22" r="8" fill="#ffe9c7" opacity=".9"/>{"".join(city)}'),
        'tulips': ('0 0 150 100', '<rect width="150" height="100" fill="url(#sc-tul-sky)"/><path d="M0 46h150v54H0z" fill="#2e9e5b"/>'
                   + ''.join(f'<path d="M{75 + (k - 4) * 4} 46 {75 + (k - 4) * 34 - 9} 100h18z" fill="{c}"/>' for k, c in enumerate(['#ff4d6d', '#ffd23f', '#ff8fab', '#9b5de5', '#ff4d6d', '#ffd23f', '#ff8fab', '#9b5de5', '#ff4d6d']))
                   + '<path d="M118 46V24" stroke="#5b4636" stroke-width="2.4"/><path d="M118 26l-8-10M118 26l10-6M118 26l6 10M118 26l-9 5" stroke="#f4f1ea" stroke-width="2"/>'),
        'coast': ('0 0 160 90', '<rect width="160" height="90" fill="url(#sc-sea)"/>'
                  '<path d="M96 0c-14 26-32 52-30 90h16c-2-34 14-62 30-90z" fill="#5fe6ea" opacity=".35"/>'
                  + ''.join(f'<path d="M{x} {y}q4-2 8 0t8 0" fill="none" stroke="#fff" stroke-width=".5" opacity=".35"/>' for x, y in [(110, 14), (128, 30), (104, 52), (132, 62), (118, 78), (142, 10), (92, 72)])
                  + '<path d="M0 0h70c-10 20-30 40-30 90H0z" fill="#2f7d4f"/><path d="M0 0h52c-8 18-22 40-20 90H0z" fill="#255f3d"/>'
                  + ''.join(f'<circle cx="{x}" cy="{y}" r="{r}" fill="{c}"/>' for x, y, r, c in [(8, 10, 6, '#1d5234'), (22, 6, 5, '#2a6b43'), (14, 26, 7, '#1d5234'), (32, 20, 5, '#2a6b43'),
                                                                                                    (6, 44, 6, '#2a6b43'), (20, 52, 7, '#1d5234'), (10, 70, 6, '#1d5234'), (26, 80, 5, '#2a6b43'), (40, 4, 4, '#1d5234')])
                  + '<path d="M70 0c-10 20-30 40-30 90h18c0-40 18-66 30-90z" fill="url(#sc-sand)"/>'
                  '<path d="M88 0c-12 24-30 50-30 90" fill="none" stroke="#fff" stroke-width="1.6" opacity=".7"/>'
                  '<path d="M91 0c-12 24-30 50-30 90" fill="none" stroke="#fff" stroke-width=".6" opacity=".4" stroke-dasharray="6 3"/>'
                  '<g transform="translate(26 40)"><path d="M110 30h6l-1.2 1.8h-3.8z" fill="#fff"/><path d="M110 30.8q-6 .8-12 3.6" stroke="#fff" stroke-width=".5" fill="none" opacity=".5"/></g>'),
        'forest': ('0 0 160 90', f'<rect width="160" height="90" fill="url(#sc-forest)"/><path d="M60 90 78 40h4l18 50z" fill="url(#sc-trail)"/>{"".join(pines)}'),
        'cover': ('0 0 100 100', '<rect width="100" height="100" fill="url(#sc-cover)"/><circle cx="78" cy="22" r="30" fill="#fff" opacity=".08"/>'
                  '<text x="9" y="38" font-size="15" font-weight="700" fill="#fff">LANDS</text><text x="9" y="55" font-size="15" font-weight="700" fill="#fff">BETWEEN</text>'
                  '<text x="9" y="70" font-size="7" font-weight="600" fill="#ffe7a3" opacity=".9" letter-spacing="1">SOUNDTRACK</text>'
                  + ''.join(f'<rect x="{9 + i * 4}" y="{88 - h}" width="2.4" height="{h}" rx="1.2" fill="#ffe7a3" opacity=".8"/>' for i, h in enumerate([4, 8, 12, 6, 10, 14, 9, 5, 11, 7, 13, 6, 9, 4, 8, 11, 6, 10, 5, 3]))),
    }
    syms = ''.join(f'<symbol id="s-{k}" viewBox="{vb}" preserveAspectRatio="xMidYMid slice">{body}</symbol>' for k, (vb, body) in s.items())
    return defs + syms


ASPECT = {'alps': 1.5, 'aurora': 1.5, 'lake': 1.5, 'city': 1.5, 'tulips': 1.5, 'blocks': 1.5, 'coast': 16 / 9, 'forest': 16 / 9, 'erdtree': 16 / 9, 'cover': 1}


# -- the files, made up
def F(name, kind, folder, size='', date='', scene=None):
    return dict(name=name, kind=kind, dir=folder, size=size, date=date, scene=scene)


def stamp(r):
    d, t = r['date'].split()
    dd, mm, yy = d.split('/')
    return yy, mm, dd, t


def by_name(rows):
    return sorted(rows, key=lambda r: (r['kind'] != 'folder', r['name'].lower()))


def by_date(rows):
    rows = sorted(rows, key=stamp, reverse=True)
    return [r for r in rows if r['kind'] == 'folder'] + [r for r in rows if r['kind'] != 'folder']


EVERYTHING = [F(n, 'folder', d, '', dt) for n, d, dt in [
    ('Apps', 'D:\\', '02/02/2026 18:20'), ('Backups', 'E:\\', '01/10/2026 03:00'), ('Clips', 'D:\\Videos', '16/10/2026 23:14'),
    ('Code', 'D:\\', '17/10/2026 08:55'), ('Discord', 'D:\\Apps', '11/10/2026 19:02'), ('Documents', 'D:\\', '15/10/2026 23:10'),
    ('Downloads', 'D:\\', '17/10/2026 09:41'), ('Elden Ring', 'D:\\Games\\Steam\\steamapps\\common', '14/10/2026 21:05'),
    ('Emulators', 'D:\\Games', '02/08/2026 18:12'), ('Epic Games', 'D:\\Games', '30/09/2026 20:01'), ('Game saves', 'D:\\Documents', '16/10/2026 22:48'),
    ('Games', 'D:\\', '14/10/2026 21:05'), ('Minecraft', 'D:\\Games', '17/10/2026 08:51'), ('Mods', 'D:\\Games', '12/10/2026 23:40'),
    ('Music', 'D:\\Media', '28/09/2026 20:33'), ('OBS recordings', 'D:\\Videos', '16/10/2026 23:14'), ('Pictures', 'D:\\', '16/10/2026 19:12'),
    ('Screenshots', 'D:\\Pictures', '17/10/2026 09:02'), ('Steam', 'D:\\Games', '14/10/2026 21:05'), ('Videos', 'D:\\', '13/10/2026 16:27')]]

POOL = [
    F('Elden Ring', 'folder', 'D:\\Games\\Steam\\steamapps\\common', '', '14/10/2026 21:05'), F('Elden Ring mods', 'folder', 'D:\\Games\\Mods', '', '12/10/2026 23:40'),
    F('Emulators', 'folder', 'D:\\Games', '', '02/08/2026 18:12'), F('Epic Games', 'folder', 'D:\\Games', '', '30/09/2026 20:01'),
    F('Field guides', 'folder', 'D:\\Documents', '', '11/09/2026 10:14'), F('Resource packs', 'folder', 'D:\\Games\\Minecraft', '', '06/10/2026 19:30'),
    F('Saves', 'folder', 'D:\\Games\\Minecraft', '', '17/10/2026 08:51'), F('Screenshots', 'folder', 'D:\\Pictures', '', '17/10/2026 09:02'),
    F('Creeper.png', 'png', 'D:\\Pictures\\Minecraft', '1.4 MB', '03/10/2026 21:12'), F('Elden Ring boss tracker.xlsx', 'xlsx', 'D:\\Documents', '18.6 KB', '15/10/2026 23:10'),
    F('Elden Ring main theme.mp3', 'mp3', 'D:\\Media\\Music\\Soundtracks', '9.8 MB', '20/09/2026 22:31'),
    F('Elden Ring map guide.pdf', 'pdf', 'D:\\Documents\\Guides', '6.2 MB', '08/10/2026 16:39'),
    F('Elden Ring no-hit Malenia.mp4', 'mp4', 'D:\\Videos\\Clips', '412 MB', '13/10/2026 16:27', 'erdtree'),
    F('Elden Ring photo mode.png', 'png', 'D:\\Pictures\\Elden Ring', '6.1 MB', '12/10/2026 22:20', 'lake'),
    F('elden_ring_backup.py', 'py', 'D:\\Code\\tools', '0.6 KB', '10/10/2026 22:03'), F('Elder Guardian farm.png', 'png', 'D:\\Pictures\\Minecraft', '3.3 MB', '01/10/2026 20:44'),
    F('Enchanting guide.pdf', 'pdf', 'D:\\Documents\\Guides', '820 KB', '27/06/2026 12:45'), F('Ender pearl stasis.mp4', 'mp4', 'D:\\Videos\\Clips', '88 MB', '30/08/2026 10:14'),
    F('Greatshield build.txt', 'txt', 'D:\\Documents\\Guides', '1.2 KB', '14/09/2026 17:48'), F('Pixel art castle.png', 'png', 'D:\\Pictures\\Minecraft', '4.4 MB', '12/05/2026 14:02'),
    F('Redstone clock.txt', 'txt', 'D:\\Games\\Minecraft\\Notes', '3.1 KB', '14/03/2026 17:48'), F('Server icon.png', 'png', 'D:\\Games\\Minecraft\\Server', '12 KB', '19/06/2026 12:10'),
    F('Speedrun Elden Ring.mp4', 'mp4', 'D:\\Videos\\Clips', '1.1 GB', '04/10/2026 21:15'), F('Steam receipts.pdf', 'pdf', 'D:\\Documents', '182 KB', '02/04/2026 08:05'),
    F('Twitch emotes.zip', 'zip', 'D:\\Downloads', '2.9 MB', '15/09/2026 11:03'),
]

QPOOL = [F('Nether fortress', 'folder', 'D:\\Games\\Minecraft\\Builds', '', '15/10/2026 22:02'), F('Banner designs.png', 'png', 'D:\\Pictures\\Minecraft', '1.2 MB', '21/07/2026 15:40'),
         F('Netherite armor.png', 'png', 'D:\\Pictures\\Minecraft', '2.6 MB', '30/05/2026 18:02'), F('Nether speedrun.mp4', 'mp4', 'D:\\Videos\\Clips', '96 MB', '12/05/2026 20:31'),
         F('Network settings.txt', 'txt', 'D:\\Games\\Minecraft\\Server', '1.1 KB', '15/10/2026 10:22'), F('New world backup.zip', 'zip', 'E:\\Backups', '1.8 GB', '01/07/2026 09:30'),
         F('Pinned seeds.txt', 'txt', 'D:\\Documents', '2 KB', '03/09/2026 19:12'), F('Snow golem.png', 'png', 'D:\\Pictures\\Minecraft', '900 KB', '10/10/2026 12:12'),
         F('Tunnel bore.png', 'png', 'D:\\Pictures\\Minecraft', '1.9 MB', '16/10/2026 07:00')]

CONTENT = by_name([
    F('Achievements guide.pdf', 'pdf', 'D:\\Documents\\Guides', '3.4 MB', '18/02/2026 10:00'), F('All-in-one modpack notes.md', 'md', 'D:\\Games\\Mods', '5.6 KB', '09/10/2026 15:58'),
    F('Base coordinates.txt', 'txt', 'D:\\Games\\Minecraft\\Notes', '1.4 KB', '14/10/2026 21:48'), F('coords.py', 'py', 'D:\\Code\\tools', '2.6 KB', '05/10/2026 21:19'),
    F('Ender dragon prep.docx', 'docx', 'D:\\Documents', '264 KB', '15/10/2026 10:22'), F('Season 3 plan.pptx', 'pptx', 'D:\\Documents', '5.6 MB', '02/10/2026 09:00'),
    F('Server chat log.txt', 'txt', 'D:\\Games\\Minecraft\\Server', '14.8 KB', '24/07/2026 18:31'), F('Speedrun route.xlsx', 'xlsx', 'D:\\Documents', '71.3 KB', '30/09/2026 17:02'),
    F('World tour script.docx', 'docx', 'D:\\Videos\\Scripts', '820 KB', '02/10/2026 09:00')])

HIRES = [
    F('Boat ride, ocean biome.mp4', 'mp4', 'D:\\Videos\\Minecraft', '1.4 GB', '13/10/2026 16:27', 'coast'),
    F('City build at night.png', 'png', 'D:\\Pictures\\Minecraft', '5.5 MB', '19/06/2026 12:10', 'city'),
    F('Erdtree at dusk.png', 'png', 'D:\\Pictures\\Elden Ring', '8.4 MB', '16/10/2026 06:58', 'erdtree'),
    F('Flower forest biome.png', 'png', 'D:\\Pictures\\Minecraft', '7.8 MB', '24/04/2026 10:31', 'tulips'),
    F('Limgrave on Torrent.mp4', 'mp4', 'D:\\Videos\\Elden Ring', '960 MB', '28/09/2026 08:12', 'forest'),
    F('Liurnia of the Lakes.png', 'png', 'D:\\Pictures\\Elden Ring', '9.1 MB', '11/08/2026 07:05', 'lake'),
    F('Mountaintops of the Giants.png', 'png', 'D:\\Pictures\\Elden Ring', '7.2 MB', '02/04/2026 18:40', 'alps'),
    F('Survival base with shaders.png', 'png', 'D:\\Pictures\\Minecraft', '6.9 MB', '19/06/2026 12:10', 'blocks')]

MEDIA = by_name(HIRES + [
    F('Creeper jump scare.mp4', 'mp4', 'D:\\Videos\\Clips', '230 MB', '18/07/2026 23:02'), F('Discord avatar.png', 'png', 'D:\\Pictures', '412 KB', '04/10/2026 21:15'),
    F('erdtree-wallpaper.jpg', 'jpg', 'D:\\Downloads', '2.1 MB', '28/09/2026 23:51'), F('Malenia attempt 47.mp4', 'mp4', 'D:\\Videos\\Clips', '640 MB', '14/10/2026 12:19'),
    F('Minecraft skin.png', 'png', 'D:\\Pictures\\Minecraft', '4 KB', '15/09/2026 11:03'), F('Screenshot 2026-10-17.png', 'png', 'D:\\Pictures\\Screenshots', '412 KB', '17/10/2026 09:02'),
    F('Webcam overlay.png', 'png', 'D:\\Pictures\\Stream', '88 KB', '30/08/2026 10:14'), F('Widescreen test.png', 'png', 'D:\\Pictures\\Screenshots', '6.1 MB', '19/06/2026 12:10'),
    F('Wither fight.mp4', 'mp4', 'D:\\Videos\\Clips', '310 MB', '07/08/2026 16:21'), F('Wolf taming.png', 'png', 'D:\\Pictures\\Minecraft', '2.7 MB', '04/10/2026 21:15')])

PICTURES = [F('Elden Ring', 'folder', 'D:\\Pictures', '', '16/10/2026 06:58', 'erdtree'), F('Minecraft', 'folder', 'D:\\Pictures', '', '17/10/2026 09:02', 'blocks'),
            F('Wallpapers', 'folder', 'D:\\Pictures', '', '19/06/2026 12:10', 'aurora')] + [r for r in HIRES if r['kind'] == 'png']

DOCUMENTS = by_date([
    F('Game saves', 'folder', 'D:\\Documents', '', '16/10/2026 22:48'), F('Mod configs', 'folder', 'D:\\Documents', '', '10/10/2026 12:12'),
    F('Elden Ring boss tracker.xlsx', 'xlsx', 'D:\\Documents', '18.6 KB', '15/10/2026 23:10'), F('Server rules.docx', 'docx', 'D:\\Documents', '36 KB', '12/10/2026 09:14'),
    F('Build plans.pdf', 'pdf', 'D:\\Documents', '1.8 MB', '09/10/2026 17:45'), F('Seed list.txt', 'txt', 'D:\\Documents', '2.4 KB', '02/10/2026 20:16'),
    F('Tournament bracket.xlsx', 'xlsx', 'D:\\Documents', '31.5 KB', '14/09/2026 17:48'), F('Résumé 2026.docx', 'docx', 'D:\\Documents', '92 KB', '02/09/2026 11:20'),
    F('Keybinds.txt', 'txt', 'D:\\Documents', '1.1 KB', '21/08/2026 19:03'), F('Stream schedule.xlsx', 'xlsx', 'D:\\Documents', '22 KB', '03/06/2026 09:41'),
    F('Steam receipts.pdf', 'pdf', 'D:\\Documents', '182 KB', '02/04/2026 08:05')])

DOWNLOADS = by_date([
    F('MosaicSetup.exe', 'exe', 'D:\\Downloads', '9.5 MB', '17/10/2026 09:41'), F('patch-notes-1.12.pdf', 'pdf', 'D:\\Downloads', '204 KB', '16/10/2026 18:02'),
    F('texture-pack-32x.zip', 'zip', 'D:\\Downloads', '148 MB', '15/10/2026 21:37'), F('malenia-guide.mp4', 'mp4', 'D:\\Downloads', '640 MB', '14/10/2026 12:19'),
    F('item-ids.csv', 'csv', 'D:\\Downloads', '2.4 MB', '13/10/2026 09:55'), F('ost-preview.mp3', 'mp3', 'D:\\Downloads', '8.2 MB', '11/10/2026 07:45'),
    F('mod-manager-setup.msi', 'msi', 'D:\\Downloads', '118 MB', '09/10/2026 22:14'), F('steam-receipt-1001.pdf', 'pdf', 'D:\\Downloads', '88 KB', '01/10/2026 08:30'),
    F('erdtree-wallpaper.jpg', 'jpg', 'D:\\Downloads', '2.1 MB', '28/09/2026 23:51')])

MUSIC = by_name([
    F('Album art', 'folder', 'D:\\Media\\Music', '', '28/09/2026 20:33'), F('Soundtracks', 'folder', 'D:\\Media\\Music', '', '20/09/2026 22:31'),
    F('Aria Math.mp3', 'mp3', 'D:\\Media\\Music', '7.4 MB', '14/06/2026 22:40'), F('Godrick the Grafted.flac', 'flac', 'D:\\Media\\Music', '38 MB', '09/01/2026 08:15'),
    F('Malenia, Blade of Miquella.flac', 'flac', 'D:\\Media\\Music', '44 MB', '17/05/2026 23:31'), F('Moog City.mp3', 'mp3', 'D:\\Media\\Music', '5.1 MB', '28/09/2026 06:52'),
    F('Subwoofer Lullaby.mp3', 'mp3', 'D:\\Media\\Music', '6.6 MB', '03/04/2026 15:20'), F('Sweden.mp3', 'mp3', 'D:\\Media\\Music', '4.9 MB', '23/02/2026 19:55')])

CODE = ['import shutil', 'from datetime import date', 'from pathlib import Path', '', 'ROAMING = Path.home() / "AppData/Roaming"',
        'SAVES = ROAMING / "EldenRing"', 'BACKUPS = Path("D:/Backups/Elden Ring")', '', '', 'def backup():', '    dest = BACKUPS / str(date.today())',
        '    shutil.copytree(SAVES, dest)', '    return dest', '', '', 'print("Backed up to", backup())']
NOTES = ['Survival world, season 3', '', '1. Coordinates', 'Base: X 412, Y 71, Z -238', 'Nether portal: X 52, Y 70, Z -30,', 'linked to the base portal. Bring',
         'fire resistance and a bow.', '', '2. To do', 'Finish the iron farm.', 'Enchant the diamond pickaxe.', '', '3. Next stream', 'Ender dragon, Saturday 8 pm.']
LENGTH = {'Elden Ring no-hit Malenia.mp4': 312, 'Limgrave on Torrent.mp4': 24, 'Elden Ring main theme.mp3': 274}


# -- the window, laid out as src/native.rs does at 100%
MENUS = ['File', 'Edit', 'View', 'Tabs', 'Search', 'Tools', 'Help']
TOGGLES = ['Content', 'Metadata', 'Match case', 'Whole word', 'Regex', 'Diacritics', 'Timestamps']
SIDE = [('All', 'folder'), ('Home', 'home'), ('Gallery', 'gallery'), None, ('Desktop', 'desktop'), ('Downloads', 'downloads'),
        ('Documents', 'documents'), ('Pictures', 'pictures'), ('Music', 'music'), ('Videos', 'videos'), None,
        ('›This PC', 'pc'), ('›Network', 'network'), ('›Linux', 'linux'), None, ('PowerShell', 'ps'), ('PowerShell (Admin)', 'ps'),
        ('Command Prompt', 'cmd'), ('Command Prompt (Admin)', 'cmd')]
SIDE_BY = {it[0].lstrip('›'): it for it in SIDE if it}
TITLE_H = 31


class Geo:
    def __init__(self, w, h, pane=350, side=186):
        self.w, self.h, self.pane, self.side = w, h, pane, side
        self.bar = h - 29
        self.pl = w - pane if pane else w
        self.ll, self.lr = side + 6, self.pl - 12
        self.menus, x = {}, 8
        for m in MENUS:
            wd = round(tw(m)) + 20
            self.menus[m] = (x, x + wd)
            x += wd
        self.strip = x + 28
        self.filter = (w - 10 - 150, w - 10)
        self.toggles, x = {}, self.filter[0] - 6
        for name in TOGGLES:  # packed from the right while the search box keeps 120 px
            wd = round(tw(name)) + 20
            if x - wd < 92 + 120 + 6:
                break
            self.toggles[name] = (x - wd, x)
            x -= wd + 6
        self.sb = (92, max(x, 92 + 120))
        self.ctl, x = {}, w - 10  # status bar controls, right to left
        self.ctl['›'] = (x - 21, x)
        x -= 21 + 6
        for name in ['Grid', 'List', 'Windows']:
            wd = round(tw(name)) + 20
            self.ctl[name] = (x - wd, x)
            x -= wd
        x -= 10
        for name, gap in [('Preview', 7), ('Smooth', 7), ('Subfolders', 2), ('Browse', 7), ('Sidebar', 10)]:
            wd = round(tw(name)) + 20
            self.ctl[name] = (x - wd, x)
            x -= wd + gap
        self.ctl['▲'] = (x - 30, x)
        self.ctl['sort'] = (x - 30 - 9 - 81, x - 30 - 9)
        self.rows, y = {}, 86
        for it in SIDE:
            if it is None:
                y += 9
                continue
            self.rows[it[0].lstrip('›')] = y
            y += 27
        room = (self.lr - self.ll) - 20 - 40 - (50 + 82 + 100)  # Name and Path share what Size, Type, Modified leave
        name = round(room * .62)
        x0 = self.ll + 10
        self.cols = [(x0, x0 + name), (x0 + name + 10, x0 + room + 10)]
        x = x0 + room + 20
        for wd in (50, 82, 100):
            self.cols.append((x, x + wd))
            x += wd + 10

    def tabs(self, labels):  # Settings › Tabs › Width: Wide (300), so a split tab's names show whole
        wd = min(300, (self.w - self.strip - 40) / len(labels))
        return [(self.strip + i * wd, self.strip + (i + 1) * wd) for i in range(len(labels))]

    def grid(self, tile):
        sx = tile + 31
        return sx, tile + 60, max(1, int((self.pl - self.side) // sx))


TR = f'translate(0 {TITLE_H})'


def toggle(p, G, name, state):
    x0, x1 = G.toggles[name]
    fill, edge, ink_ = {'on': (p.hl, p.accent, p.accent), 'off': (p.card, p.line, p.muted), 'hover': (p.row, mix(p.line, p.muted, .5), p.text)}[state]
    return rect(x0, 44, x1 - x0, 27, fill, 4, edge) + text((x0 + x1) / 2, 57.5, name, ink_, anchor='middle')


def ctl_button(p, G, name, state):
    x0, x1 = G.ctl[name]
    fill, edge, ink_ = {'on': (p.hl, p.accent, p.accent), 'off': (p.card, p.line, p.muted), 'hover': (p.row, mix(p.line, p.muted, .5), p.text)}[state]
    return rect(x0, G.bar + 4, x1 - x0, 21, fill, 4, edge) + text((x0 + x1) / 2, G.bar + 14.5, name, ink_, anchor='middle')


def side_row(p, G, it, y):
    label, ic = it
    o = chevron_right(17, y + 13.5, p.muted) if label.startswith('›') else ''
    return o + use('i-' + ic, 30, y + 5.5, 16) + text(54, y + 13.5, fit(label.lstrip('›'), G.side - 62), p.text)


def piece_frame(p, G):
    """What never changes: the frame, title bar, menu names, toolbar, places and status bar."""
    w, h, bar = G.w, G.h, G.bar
    o = [rect(0, 0, w, h + TITLE_H, p.bg, 8), use('i-logo', 10, 7, 17),
         f'<g stroke="{p.text}" fill="none"><path d="M{w - 120} 15.5h10"/><rect x="{w - 73.5}" y="10.5" width="9" height="9" rx="1.5"/>'
         f'<path d="M{w - 27.5} 10.5l9 9m0-9-9 9"/></g>', f'<g transform="{TR}">']
    for m, (x0, x1) in G.menus.items():
        o.append(text(x0 + 10, 18.5, m, p.text))
    o.append(rect(0, 37, w, 42, p.card) + hline(0, w, 37, p.line) + hline(0, w, 78, p.line))
    ar = 'fill="none" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"'
    o.append(f'<path d="M33 57.5H20m5.5-5.5L20 57.5l5.5 5.5" stroke="{p.text}" {ar}/><path d="M55 57.5h13m-5.5-5.5 5.5 5.5-5.5 5.5" stroke="{p.muted}" {ar}/>')
    o.append(rect(G.sb[0], 44, G.sb[1] - G.sb[0], 27, p.bg, 4, p.line))
    for name in G.toggles:
        o.append(toggle(p, G, name, 'off'))
    f0, f1 = G.filter
    o.append(rect(f0, 44, f1 - f0, 27, p.bg, 4, p.line) + text(f0 + 10, 57.5, 'Everything', p.text) + chevron_down(f1 - 16, 57.5, p.text))
    o.append(rect(0, 79, G.side, bar - 79, p.card) + vline(G.side - 1, 79, bar, p.line))
    y = 86
    for it in SIDE:
        if it is None:
            o.append(hline(10, G.side - 10, y + 4, p.line))
            y += 9
            continue
        if y + 27 > bar - 61:
            break
        o.append(side_row(p, G, it, y))
        y += 27
    o.append(hline(10, G.side - 10, bar - 61, p.line) + use('i-drive', 20, bar - 52, 32) + text(60, bar - 42, 'Local Disk (C:)', p.text))
    o.append(rect(60, bar - 29, 102, 4, p.line, 2) + rect(60, bar - 29, 68, 4, '#d4a72c', 2))
    o.append(text(60, bar - 17, '318 GB free', p.muted, size=9) + text(162, bar - 17, '953 GB', p.muted, size=9, anchor='end'))
    if G.pane:
        o.append(rect(G.pl, 79, G.pane, bar - 79, p.card) + vline(G.pl, 79, bar, p.line))
    o.append(f'<path d="M0 {bar}H{w}V{h - 8}a8 8 0 0 1-8 8H8a8 8 0 0 1-8-8z" fill="{p.card}"/>' + hline(0, w, bar, p.line))
    for name in ['Sidebar', 'Browse', 'Subfolders', 'Smooth', 'Preview']:
        o.append(ctl_button(p, G, name, 'on' if name in ('Sidebar', 'Browse') or (name == 'Preview' and G.pane) else 'off'))
    x0, x1 = G.ctl['›']
    o.append(rect(x0, bar + 4, 21, 21, p.card, 4, p.line) + chevron_right((x0 + x1) / 2, bar + 14.5, p.muted) + '</g>')
    o.append(rect(0, 0, w, h + TITLE_H, None, 8, mix(p.line, p.muted, .25)))
    return ''.join(o)


def piece_title(p, G, st):
    name = st['tab_labels'][st['tab']]
    if isinstance(name, tuple):  # a split tab: its focused pane's
        name = name[st['focus_pane']]
    return text(36, 15.5, f'{name} — Mosaic', p.text)


def fair(want, room):
    """native_tabs.rs fair: widths for pieces wanting `want` in `room`: those that fit whole keep their
    width, the rest share what's left."""
    out, left = [0] * len(want), room
    for n, i in enumerate(sorted(range(len(want)), key=lambda i: want[i])):
        out[i] = min(want[i], left / (len(want) - n))
        left -= out[i]
    return out


assert fair([30, 200], 150) == [30, 120] and fair([30, 40], 150) == [30, 40] and fair([200, 200], 100) == [50, 50]


def tab_face(p, x0, x1, label, ic, active, close):
    cy = 22.5 if active else 20.5
    c, wt = (p.text, 600) if active else (p.muted, 400)
    o = use('i-' + ic, x0 + 12, cy - 8, 16)
    if isinstance(label, tuple):
        # a split tab (native_tabs.rs): its layout, small, then each pane's name side by side
        lc, x = (p.accent if active else p.muted), x0 + 41.5
        o += rect(x0 + 34, cy - 5.5, 14, 11, None, 1.5, lc) + f'<path d="M{num(x)} {num(cy - 5.5)}V{num(cy + 5.5)}'
        o += (f'M{num(x)} {num(cy)}H{num(x0 + 47.5)}' if len(label) > 2 else '') + f'" stroke="{lc}"/>'
        tx, gap = x0 + 54, 13
        for i, (s, sw) in enumerate(zip(label, fair([tw(s, w=wt) + 1 for s in label], x1 - 28 - tx - gap * (len(label) - 1)))):
            if i:
                o += rect(tx - gap / 2 - 1, cy - 6, 1, 12, p.line)
            o += text(tx, cy, fit(s, sw, w=wt), c, w=wt)
            tx += sw + gap
    else:
        o += text(x0 + 34, cy, fit(label, x1 - x0 - 34 - 28, w=wt), c, w=wt)
    if close:
        o += f'<path d="M{num(x1 - 21.5)} {num(cy - 4)}l8 8m0-8-8 8" stroke="{c}" stroke-linecap="round"/>'
    return o


def piece_strip(p, G, st):
    labels, icons, cur, hv, ghost = st['tab_labels'], st['tab_icons'], st['tab'], st.get('tab_hover'), st.get('ghost', ())
    xs = G.tabs(labels)
    o = []
    for i, (x0, x1) in enumerate(xs):
        if i == cur or i in ghost:
            continue
        if i == hv:
            o.append(rect(x0 + 2, 8, x1 - x0 - 4, 25, p.hl, 7))
        o.append(tab_face(p, x0, x1, labels[i], icons[i], False, i == hv))
    for i in range(len(xs) - 1):
        if not {i, i + 1} & ({cur, hv} | set(ghost)):
            o.append(vline(xs[i][1], 13, 28, p.line))
    if cur not in ghost:
        x0, x1 = xs[cur]
        o.append(f'<path d="{tab_path(x0, x1, 6, 38)}Z" fill="{p.card}"/><path d="{tab_path(x0 + .5, x1 - .5, 6.5, 37.5)}" fill="none" stroke="{p.line}"/>')
        o.append(tab_face(p, x0, x1, labels[cur], icons[cur], True, True))
    px = xs[-1][1] + 6
    o.append(f'<path d="M{num(px + 14)} 15v12M{num(px + 8)} 21h12" stroke="{p.muted}" stroke-width="1.2" stroke-linecap="round"/>')
    return g(''.join(o), TR)


def piece_search(p, G, st):
    x0, x1 = G.sb
    o = []
    if st.get('focus'):
        o.append(rect(x0, 44, x1 - x0, 27, p.bg, 4, p.accent))
    q, tx = st.get('q', ''), x0 + 9
    if q and st.get('q_sel'):
        o.append(rect(tx - 1, 49, tw(q) + 2, 17, '#0078d4') + text(tx, 57.5, q, '#ffffff'))
    elif q:
        o.append(text(tx, 57.5, fit(q, x1 - x0 - 18), p.text))
    if st.get('focus') and not st.get('q_sel'):
        o.append(f'<rect class="caret" x="{num(tx + tw(q) + .5)}" y="50" width="1" height="15" fill="{p.text}"/>')
    for name in st.get('on', ()):
        if name in G.toggles:
            o.append(toggle(p, G, name, 'on'))
    hv = st.get('hover_toggle')
    if hv and hv not in st.get('on', ()):
        o.append(toggle(p, G, hv, 'hover'))
    return g(''.join(o), TR) if o else ''


def piece_side(p, G, st):
    o = []
    cur, hv = st.get('side'), st.get('side_hover')
    if hv and hv != cur:
        y = G.rows[hv]
        o.append(rect(6, y, G.side - 12, 27, p.row, 6) + side_row(p, G, SIDE_BY[hv], y))
    if cur:
        y = G.rows[cur]
        o.append(rect(6, y, G.side - 12, 27, mix(p.card, p.hl, .85), 6) + rect(6, y + 5.5, 3, 16, p.accent, 1.5) + side_row(p, G, SIDE_BY[cur], y))
    return g(''.join(o), TR) if o else ''


def runs(name, terms):
    """[(text, matched)]: where any term occurs, ignoring case."""
    low, mark = name.lower(), [False] * len(name)
    for t in terms:
        t, i = t.lower(), 0
        while t and (i := low.find(t, i)) >= 0:
            mark[i:i + len(t)] = [True] * len(t)
            i += len(t)
    out = []
    for ch, m in zip(name, mark):
        if out and out[-1][1] == m:
            out[-1][0] += ch
        else:
            out.append([ch, m])
    return out


def name_text(p, x, cy, name, width, terms):
    parts = ''.join(f'<tspan font-weight="700" fill="{p.accent}">{esc(t)}</tspan>' if m else esc(t) for t, m in runs(fit(name, width), terms))
    return text(x, cy, parts, p.text, raw=True)


def wrap(s, width):
    if tw(s) <= width:
        return [s]
    lines, cur = [], ''
    for wd in s.split(' '):
        t = (cur + ' ' + wd).strip()
        if tw(t) <= width or not cur:
            cur = t
        else:
            lines.append(cur)
            cur = wd
    lines.append(cur)
    if len(lines) > 2:
        lines = [lines[0], ' '.join(lines[1:])]
    return [fit(ln, width) for ln in lines]


def big_folder(cx, cy, size, inner=None):
    """Windows' large folder: the back, a picture peeking out, the front."""
    k = size / 64
    peek = (f'<use href="#s-{inner}" x="9" y="13" width="46" height="31" transform="rotate(-4 32 28)"/>' if inner else
            '<rect x="10" y="14" width="44" height="26" rx="2" fill="#fff"/>')
    return (f'<g transform="translate({num(cx - 32 * k)} {num(cy - 32 * k)}) scale({num(k)})">'
            f'<path d="M4 14a4 4 0 0 1 4-4h14.3a4 4 0 0 1 2.9 1.2l3.3 3.3H56a4 4 0 0 1 4 4V24H4z" fill="#e39b17"/>{peek}'
            '<path d="M4 23.5A2.5 2.5 0 0 1 6.5 21h51a2.5 2.5 0 0 1 2.5 2.5V50a4 4 0 0 1-4 4H8a4 4 0 0 1-4-4z" fill="url(#g-fold)"/>'
            '<path d="M6 22h52" stroke="#ffe49a"/></g>')


def thumb(r, cx, cy, size):
    """A tile's picture: the photo or video frame contained in the tile, else a big icon."""
    sc = r.get('scene')
    if r['kind'] == 'folder':
        return big_folder(cx, cy, size, sc)
    if sc:
        w, h = size, size / ASPECT[sc]
        return f'<use href="#s-{sc}" x="{num(cx - w / 2)}" y="{num(cy - h / 2)}" width="{num(w)}" height="{num(h)}"/>'
    return use('i-' + ICON_OF[r['kind']], cx - size * .4, cy - size * .4, size * .8)


def list_header(p, G, sort):
    x0, x1 = G.ll, G.lr
    o = [rect(x0, 87, x1 - x0, 30, p.card, 8, p.line)]
    for k, ((a, b), label) in enumerate(zip(G.cols, ['Name', 'Path', 'Size', 'Type', 'Modified'])):
        if k:
            o.append(vline(a - 6, 93, 111, p.line))
        if k == 2:
            o.append(text(b, 102, label, p.muted, anchor='end'))
        else:
            o.append(text(a, 102, label, p.muted))
            if sort[0] == k:
                o.append(tri(a + tw(label) + 10, 102, p.accent, up=sort[1] == 'asc'))
    return ''.join(o)


def pane_rects(G, n):
    """A split tab's panes, 8 px apart: side by side, then the right one halved."""
    l, t, r, b = G.side, 87, G.pl, G.bar
    mid, my = (l + r) // 2, (t + b) // 2
    return [(l, t, mid - 4, b), (mid + 4, t, r, b)] if n == 2 else [(l, t, mid - 4, b), (mid + 4, t, r, my - 4), (mid + 4, my + 4, r, b)]


def piece_panes(p, G, st):
    """A split tab (native.rs paint_head): each pane's caption, the focused one standing out, then its list.
    At this width fit_cols keeps the Name column alone."""
    o = []
    for k, ((name, rows, sort), (l, t, r, b)) in enumerate(zip(st['panes'], pane_rects(G, len(st['panes'])))):
        on, cl, cy = k == st['focus_pane'], l + 6, t + 13.5
        if on:
            o.append(rect(cl, t, r - 4 - cl, 27, p.card, 6, p.line) + text(r - 4 - 12, cy, '×', p.muted, anchor='middle'))
        o.append(f'<circle cx="{num(cl + 12)}" cy="{num(cy)}" r="3" fill="{p.accent if on else p.line}"/>' + text(cl + 22, cy, name, p.text if on else p.muted))
        top, x1 = t + 31, r - 12
        o.append(rect(cl, top, x1 - cl, 30, p.card, 8, p.line) + text(cl + 10, top + 15, 'Name', p.muted))
        if sort[0] == 0:
            o.append(tri(cl + 10 + tw('Name') + 10, top + 15, p.accent, up=sort[1] == 'asc'))
        for i, row in enumerate(rows):
            ry = top + 44 + i * 28
            if ry + 14 > b:
                break
            o.append(use('i-' + ICON_OF[row['kind']], cl + 10, ry - 8, 16) + name_text(p, cl + 36, ry, row['name'], x1 - cl - 46, ()))
    return g(''.join(o), TR)


def piece_list(p, G, st):
    if st.get('panes'):
        return piece_panes(p, G, st)
    rows, terms = st['rows'], st.get('terms', ())
    if st.get('view', 'list') == 'list':
        o, c = [list_header(p, G, st.get('sort', (0, 'asc')))], G.cols
        for i, r in enumerate(rows):
            cy = 131 + i * 28
            if cy - 14 > G.bar:
                break
            o.append(use('i-' + ICON_OF[r['kind']], c[0][0], cy - 8, 16) + name_text(p, c[0][0] + 26, cy, r['name'], c[0][1] - c[0][0] - 26, terms))
            o.append(text(c[1][0], cy, fit(r['dir'], c[1][1] - c[1][0]), p.muted))
            if r['size']:
                o.append(text(c[2][1], cy, r['size'], p.muted, anchor='end'))
            o.append(text(c[3][0], cy, fit(TYPE_OF[r['kind']], c[3][1] - c[3][0]), p.muted) + text(c[4][0], cy, fit(r['date'], c[4][1] - c[4][0]), p.muted))
        return g(''.join(o), TR, extra=f' clip-path="url(#clip-list-{G.w})"')
    tile = st.get('tile', 72)
    sx, sy, cols = G.grid(tile)
    o = []
    for i, r in enumerate(rows):
        cx, top = G.side + (i % cols) * sx + sx / 2, 97 + (i // cols) * sy
        o.append(thumb(r, cx, top + tile / 2, tile))
        for k, ln in enumerate(wrap(r['name'], sx - 10)):
            o.append(name_text(p, cx - tw(ln) / 2, top + tile + 13 + k * 17, ln, sx, terms))
    return g(''.join(o), TR)


def piece_marks(p, G, st):
    """Selection and hover, under the list's rows or the grid's tiles."""
    o = []
    for kind, i in (('hover', st.get('hover')), ('sel', st.get('sel'))):
        if i is None or (kind == 'hover' and i == st.get('sel')):
            continue
        fill = p.sel if kind == 'sel' else p.row
        if st.get('view', 'list') == 'list':
            o.append(rect(G.ll, 117 + i * 28 + 2, G.lr - G.ll, 24, fill, 6))
        else:
            tile = st.get('tile', 72)
            sx, sy, cols = G.grid(tile)
            x, top = G.side + (i % cols) * sx, 97 + (i // cols) * sy
            lines = len(wrap(st['rows'][i]['name'], sx - 10))
            o.append(rect(x + 5, top - 7, sx - 10, tile + 34 + (lines - 1) * 17, fill, 8, p.accent if kind == 'sel' else None))
    return g(''.join(o), TR) if o else ''


def piece_status(p, G, st):
    bar, grid = G.bar, st.get('view') == 'grid'
    o = [text(10, bar + 14.5, fit(st['status'], (G.ctl['sort'][0] if grid else G.ctl['Sidebar'][0]) - 20), p.muted)]
    mode, hv = st.get('mode', 'List'), st.get('ctl_hover')
    a, b = G.ctl['Windows'][0], G.ctl['Grid'][1]
    o.append(rect(a, bar + 4, b - a, 21, p.card, 4, p.line))
    for name in ('Windows', 'List', 'Grid'):
        x0, x1 = G.ctl[name]
        if name != 'Grid':
            o.append(vline(x1, bar + 8, bar + 21, p.line))
        if name == mode:
            o.append(rect(x0, bar + 4, x1 - x0, 21, p.hl, 4, p.accent))
        elif name == hv:
            o.append(rect(x0 + 1, bar + 5, x1 - x0 - 2, 19, p.row, 3))
        o.append(text((x0 + x1) / 2, bar + 14.5, name, p.accent if name == mode else p.text if name == hv else p.muted, anchor='middle'))
    if grid:
        a, b = G.ctl['sort']
        o.append(rect(a, bar + 4, b - a, 21, p.bg, 4, p.line) + text(a + 9, bar + 14.5, 'Name', p.text) + chevron_down(b - 13, bar + 14.5, p.text))
        a, b = G.ctl['▲']
        o.append(rect(a, bar + 4, b - a, 21, p.card, 4, p.line) + tri((a + b) / 2, bar + 14.5, p.text))
    return g(''.join(o), TR)


# -- the preview pane
def clock(s):
    s = int(s)
    return f'{s // 60}:{s % 60:02d}'


def media_bar(p, x0, y, w, t0, t1, length):
    """▶/❚❚ · seek · time · speaker · volume (native_preview.rs), the seek knob and the clock running."""
    cy = y + 14.5
    vol0 = x0 + w - 64
    mute0 = vol0 - 6 - 29
    a, b = x0 + 29 + 12, mute0 - tw('0:00:00 / 0:00:00') - 24
    o = [rect(x0, y, 29, 29, p.bg, 6, p.line), f'<path d="M{num(x0 + 11.5)} {num(cy - 5)}v10M{num(x0 + 17.5)} {num(cy - 5)}v10" stroke="{p.text}" stroke-width="2.2"/>',
         rect(mute0, y, 29, 29, p.bg, 6, p.line),
         f'<path d="M{num(mute0 + 7)} {num(cy - 2.5)}h3l4-3.5v12l-4-3.5h-3z" fill="{p.text}"/><path d="M{num(mute0 + 17)} {num(cy - 3)}q2.2 3 0 6M{num(mute0 + 19.5)} {num(cy - 5.5)}q4.4 5.5 0 11" fill="none" stroke="{p.text}" stroke-width="1.1" stroke-linecap="round"/>',
         rect(vol0, cy - 2, 64, 4, p.muted, 2, op=.35) + rect(vol0, cy - 2, 45, 4, p.accent, 2) + f'<circle cx="{num(vol0 + 45)}" cy="{num(cy)}" r="6" fill="{p.accent}"/>',
         rect(a, cy - 2, b - a, 4, p.muted, 2, op=.35)]
    f1 = (t1 - t0) / length
    sx = A.track('transform:scaleX({})', [(t0, .001), (t1, f1), (t1 + .5, f1)], 'linear')
    kx = A.track('transform:translateX({}px)', [(t0, 0), (t1, f1 * (b - a)), (t1 + .5, f1 * (b - a))], 'linear')
    o.append(f'<g transform="translate({num(a)} {num(cy - 2)})"><rect class="{sx}" width="{num(b - a)}" height="4" rx="2" fill="{p.accent}"/></g>')
    o.append(f'<circle class="{kx}" cx="{num(a)}" cy="{num(cy)}" r="6" fill="{p.accent}"/>')
    for k in range(int(t1 - t0) + 1):
        o.append(A.show(text(b + 12, cy, f'{clock(k)} / {clock(length)}', p.text), [(t0 + k, min(t0 + k + 1, t1 + .3), 0, 0)]))
    return ''.join(o)


def video(sc, x, y, w, h, t0, t1):
    """The clip playing: a slow push in, and something moving in it."""
    vh = w / ASPECT[sc]
    kb = A.track('transform:scale({})', [(t0, 1), (t1, 1.09), (t1 + .5, 1.09)], 'linear')
    if sc == 'coast':
        m = A.track('transform:translate({}px,{}px)', [(t0, (0, 0)), (t1, (16, -5)), (t1 + .5, (16, -5))], 'linear')
        fx = (f'<g class="{m}"><path d="M110 30h8l-1.6 2.4h-5z" fill="#fff"/>'
              '<path d="M110 31.3q-9 1-19 5.5M110.5 32q-7 2.4-12.5 7.5" stroke="#fff" stroke-width=".7" fill="none" opacity=".6"/></g>')
    else:
        o1 = A.track('opacity:{}', [(t0, .25), ((t0 + t1) / 2, .65), (t1, .35)])
        fx = f'<path class="{o1}" d="M98 0h16L86 90H70z" fill="url(#sc-ray)"/><path class="{o1}" d="M122 0h8L100 90h-8z" fill="url(#sc-ray)" opacity=".7"/>'
    inner = f'<use href="#s-{sc}" width="160" height="90"/>{fx}'
    return (f'<svg x="{num(x)}" y="{num(y + (h - vh) / 2)}" width="{num(w)}" height="{num(vh)}" viewBox="0 0 160 90">'
            f'{about(80, 45, kb, inner)}</svg>')


def excel(x, y, w, h):
    """The Excel preview handler: a light sheet, whatever the theme."""
    o = [rect(x, y, w, h, '#ffffff')]
    rw, hh, rh = 26, 20, 20
    cols = [('A', 92, 'Boss'), ('B', 44, 'Tries'), ('C', 44, 'Level'), ('D', 68, 'Runes'), ('E', w - rw - 248, '')]
    data = [('Margit', 7, 28, 12000), ('Godrick', 4, 34, 20000), ('Rennala', 3, 45, 40000), ('Radahn', 12, 62, 70000), ('Morgott', 9, 80, 120000),
            ('Malenia', 47, 118, 480000)]
    foot = h - 22
    o.append(rect(x, y, w, hh, '#f3f3f3') + rect(x, y, rw, foot, '#f3f3f3'))
    xs, cx = [], x + rw
    for name, cw, _ in cols:
        xs.append((cx, cw))
        o.append(text(cx + cw / 2, y + hh / 2, name, '#444', size=11, anchor='middle') + vline(cx + cw - 1, y, y + foot, '#d4d4d4'))
        cx += cw
    for k in range(int((foot - hh) // rh)):
        ry = y + hh + k * rh
        o.append(hline(x, x + w, ry + rh - 1, '#e1e1e1') + text(x + rw / 2, ry + rh / 2, str(k + 1), '#444', size=11, anchor='middle'))
        cells = None
        if k == 0:
            cells = [c[2] for c in cols]
        elif k <= len(data):
            boss, tries, level, runes = data[k - 1]
            cells = [boss, str(tries), str(level), f'{runes:,}', '']
        elif k == len(data) + 2:
            cells = ['Total', str(sum(d[1] for d in data)), '', f'{sum(d[3] for d in data):,}', '']
        for j, v in enumerate(cells or ()):
            if v:
                bold = 600 if k in (0, len(data) + 2) else 400
                cx, cw = xs[j]
                o.append(text(cx + 4, ry + rh / 2, v, '#111', size=11, w=bold) if j == 0 or k == 0 else
                         text(cx + cw - 5, ry + rh / 2, v, '#111', size=11, w=bold, anchor='end'))
    o.append(hline(x, x + w, y + hh - 1, '#c8c8c8') + vline(x + rw - 1, y, y + foot, '#c8c8c8'))
    o.append(rect(xs[0][0] - 1, y + hh - 1, xs[0][1] + 1, rh + 1, None, 0, '#107c41', sw=2))
    o.append(rect(x, y + foot, w, 22, '#f3f3f3') + hline(x, x + w, y + foot, '#d4d4d4') + text(x + 14, y + foot + 11, '‹  ›', '#777', size=11))
    o.append(rect(x + 46, y + foot, 56, 22, '#fff') + text(x + 74, y + foot + 11, 'Bosses', '#107c41', size=11, w=600, anchor='middle') + rect(x + 50, y + h - 3, 48, 2, '#107c41'))
    return ''.join(o)


def pdf_page(x, y, w, h):
    """The PDF preview handler: a page on grey."""
    o = [rect(x, y, w, h, '#3b3b3b')]
    pw = w - 28
    ph = min(h - 20, pw * 1.414)
    px, py = x + 14, y + 10
    o.append(rect(px + 1, py + 2, pw, ph, '#000', op=.3) + rect(px, py, pw, ph, '#ffffff'))
    lx, lw = px + 20, pw - 40
    o.append(rect(lx, py + 24, 6, 6, '#a16207', 1) + text(lx + 11, py + 27, 'TARNISHED GUIDES', '#a16207', size=7, w=600))
    o.append(text(lx, py + 50, 'Limgrave map guide', '#111827', size=15, w=600) + text(lx, py + 68, 'Sites of Grace, bosses and secrets · v1.4', '#6b7280', size=8))
    yy = py + 86
    for k, f in enumerate([.95, .9, .97, .6]):
        o.append(rect(lx, yy + k * 9, lw * f, 3.4, '#d1d5db', 1.7))
    yy += 48
    o.append(rect(lx, yy, lw, 18, '#f3f4f6') + text(lx + 6, yy + 9, 'Area', '#374151', size=7.5, w=600) + text(lx + lw - 6, yy + 9, 'Graces', '#374151', size=7.5, w=600, anchor='end'))
    for k, (a, v) in enumerate([('Stormhill', '4'), ('Weeping Peninsula', '9'), ('Stormveil Castle', '6'), ('Mistwood', '3')]):
        ry = yy + 18 + k * 16
        o.append(hline(lx, lx + lw, ry + 15, '#e5e7eb') + text(lx + 6, ry + 8, a, '#374151', size=7.5) + text(lx + lw - 6, ry + 8, v, '#111827', size=7.5, anchor='end'))
    yy += 18 + 4 * 16 + 16
    for k, (f, c) in enumerate([(.62, '#a16207'), (.27, '#ca8a04'), (.21, '#eab308'), (.18, '#fde047')]):
        o.append(rect(lx, yy + k * 11, (lw - 50) * f + 10, 6, c, 3))
    yy += 56
    for k, f in enumerate([.97, .92, .95, .88, .7, .94, .9, .6]):
        if yy + k * 9 < py + ph - 24:
            o.append(rect(lx, yy + k * 9, lw * f, 3.4, '#d1d5db', 1.7))
    o.append(text(px + pw / 2, py + ph - 10, '1 / 12', '#9ca3af', size=7, anchor='middle'))
    return ''.join(o)


def preview(r, t0=None, t1=None):
    k = r['kind']
    kind = {'xlsx': 'xlsx', 'pdf': 'pdf', 'jpg': 'img', 'png': 'img', 'py': 'text', 'txt': 'text', 'mp4': 'video', 'mp3': 'audio'}[k]
    d = dict(kind=kind, row=r)
    if kind == 'text':
        d['lines'] = CODE if k == 'py' else NOTES
    if kind in ('video', 'audio'):
        d.update(t0=t0, t1=t1)
    return d


def piece_pane(p, G, st):
    if not G.pane:
        return ''
    pv = st.get('pv')
    if not pv:
        return g(text((G.pl + G.w) / 2, (79 + G.bar) / 2, 'Select a file to preview', p.muted, anchor='middle'), TR)
    r, kind = pv['row'], pv['kind']
    x0, x1 = G.pl + 20, G.w - 12
    top, bottom = 135, G.bar - 12
    w, h = x1 - x0, bottom - top
    o = [text(x0, 99.5, fit(r['name'], w, w=600), p.text, w=600),
         text(x0, 116.5, fit(' · '.join(v for v in (r['size'], r['date'], r['dir']) if v), w), p.muted)]
    if kind == 'xlsx':
        o.append(excel(x0, top, w, h))
    elif kind == 'pdf':
        o.append(pdf_page(x0, top, w, h))
    elif kind == 'img':
        ih = w / ASPECT[r['scene']]
        o.append(f'<use href="#s-{r["scene"]}" x="{num(x0)}" y="{num(top + (h - ih) / 2)}" width="{num(w)}" height="{num(ih)}"/>')
    elif kind == 'text':
        o += [text(x0, top + 8 + k * 15, ln, p.text, mono=True) for k, ln in enumerate(pv['lines']) if ln]
    else:
        ctl = bottom - 29
        body = ctl - 8 - top
        if kind == 'video':
            o.append(video(r['scene'], x0, top, w, body, pv['t0'], pv['t1']))
        else:
            s = min(w, body)
            o.append(use('s-cover', x0 + (w - s) / 2, top + (body - s) / 2, s))
        o.append(media_bar(p, x0, ctl, w, pv['t0'], pv['t1'], LENGTH[r['name']]))
    return g(''.join(o), TR)


# -- menus (native.rs paint_menu)
def menu(p, x, y, items, hover=None):
    """items: (label, keys, submenu, disabled, checked), None for a line. Returns markup, the rows, the width."""
    pad, top = 5, 3
    label_w = max(tw(it[0]) for it in items if it)
    key_w = max(tw(it[1]) for it in items if it)
    arrow = 18 if any(it and it[2] for it in items) else 0
    gutter = 24 if any(it and it[4] for it in items) else 0
    wd = round(max(210, 2 * pad + 20 + gutter + label_w + arrow + (28 + key_w if key_w else 0)))
    rows, yy = [], top
    for it in items:
        hh = 9 if it is None else 22
        rows.append((yy, yy + hh))
        yy += hh
    ht = yy + top
    o = [shadow(x, y, wd, ht, 12, .42, 4), rect(x, y, wd, ht, p.card, 6, mix(p.line, p.muted, .35))]
    for k, (it, (a, b)) in enumerate(zip(items, rows)):
        if it is None:
            o.append(hline(x + pad + 6, x + wd - pad - 6, y + a + 4, p.line))
            continue
        label, keys, sub, off, on = it
        cy = y + (a + b) / 2
        if k == hover:
            o.append(rect(x + pad, y + a, wd - 2 * pad, b - a, p.hl, 3, mix(p.hl, p.accent, .35)))
        if on:
            o.append(text(x + pad + 8, cy, '✓', p.accent))
        o.append(text(x + pad + 10 + gutter, cy, label, p.muted if off else p.text))
        right = x + wd - pad - 10
        if sub:
            o.append(chevron_right(right - 2, cy, p.muted if off else p.text))
            right -= 18
        if keys:
            o.append(text(right, cy, keys, p.muted, anchor='end'))
    return ''.join(o), [(y + a, y + b) for a, b in rows], wd


TABS_MENU = [('New tab', 'Ctrl+T', 0, 0, 0), ('Reopen closed tab', 'Ctrl+Shift+T', 0, 1, 0), ('Recently closed', '', 0, 1, 0), ('Find tab…', 'Ctrl+Shift+A', 0, 0, 0), None,
             ('This tab', '', 1, 0, 0), ('Layout', '', 1, 0, 0), ('Groups', '', 0, 1, 0), None, ('Workspaces', '', 1, 0, 0), ('Windows', '', 1, 0, 0),
             ('At startup', '', 1, 0, 0), None, ('Appearance', '', 1, 0, 0), ('Ctrl+Tab in recently used order', '', 0, 0, 0), ('Sleeping tabs', '', 1, 0, 0),
             ('Keyboard shortcuts', '', 1, 0, 0)]
WINDOWS_MENU = [('Downloads — Mosaic', '', 0, 0, 0), None, ('Merge all windows here', '', 0, 0, 0)]
VIEW_MENU = [('List', '', 0, 0, 1), ('Grid', '', 0, 0, 0), None, ('Preview', 'Alt+P', 0, 0, 1), ('Smooth', '', 0, 0, 0), ('Sidebar', 'Alt+B', 0, 0, 1), None,
             ('Larger icons', 'Ctrl++', 0, 0, 0), ('Smaller icons', 'Ctrl+-', 0, 0, 0), None, ('Theme…', '', 0, 0, 0), ('Icons…', '', 0, 0, 0), None,
             ('Show hidden files', '', 0, 0, 1), ('Colour folder names', '', 0, 0, 0), ('Show protected system files', '', 0, 0, 0)]
MENU_Y = 36


def piece_menu(p, G, st):
    m, hb = st.get('menu'), st.get('menubar_hover')
    name = m[0] if m else hb
    if not name:
        return ''
    x0, x1 = G.menus[name]
    o = [rect(x0, 6, x1 - x0, 25, p.hl, 5) + text(x0 + 10, 18.5, name, p.text)]
    if m:
        mk, rows, wd = menu(p, x0, MENU_Y, TABS_MENU if name == 'Tabs' else VIEW_MENU, m[1])
        o.append(mk)
        if len(m) > 2:
            o.append(menu(p, x0 + wd, rows[10][0] - 3, WINDOWS_MENU, m[2])[0])
    return g(''.join(o), TR)


# -- the theme picker (native_themes.rs), drawn at 1x then scaled onto the card
DLG_W, DLG_H, DLG_K = 676, 724, .84


def cell(i):
    sec, k = divmod(i, 8)
    row, col = divmod(k, 4)
    return 24 + col * 160, 64 + (sec + 1) * 34 + sec * 256 + row * 124


def mini_defs():
    """Each theme's miniature, once: a window in its colours (native_themes.rs mini)."""
    out = ['<clipPath id="mini-clip"><rect width="148" height="86" rx="6"/></clipPath>']
    for name in THEMES:
        q = Pal(name)

        def bar(x, y, w, c, a):
            return f'<rect x="{num(x)}" y="{num(y)}" width="{num(w)}" height="4" rx="2" fill="{c}" opacity="{num(a)}"/>'
        rr, b, strip = 148, 86, 14
        tool, st, side = strip + 14, b - 9, 34
        s = [rect(0, 0, 148, 86, q.bg), f'<path d="{tab_path(12, 52, 4, strip, 3, 3)}Z" fill="{q.card}"/>',
             bar(18, 7.5, 22, q.text, .8), bar(60, 7.5, 20, q.muted, .6),
             rect(0, strip, 148, 14, q.card), rect(0, tool, 148, 1, q.line),
             rect(26, strip + 3, rr - 56, 8, q.bg, 2, q.line), rect(rr - 26, strip + 3, 18, 8, q.hl, 2, q.accent),
             rect(0, st, 148, 9, q.card), rect(0, st, 148, 1, q.line),
             rect(0, tool + 1, side, st - tool - 1, q.card), rect(side - 1, tool + 1, 1, st - tool - 1, q.line)]
        for k in range(3):
            y = tool + 6 + 8 * k
            if k == 1:
                s.append(rect(3, y - 2, side - 7, 8, q.hl, 2))
            s.append(bar(7, y, 20, q.text if k == 1 else q.muted, .7))
        for k in range(4):
            y = tool + 5 + 9.5 * k
            if k == 1:
                s.append(rect(side + 4, y - 2.5, rr - side - 8, 9, q.sel, 2))
            s.append(bar(side + 8, y, 30 + 6 * ((k * 5) % 3), q.accent if k == 0 else q.text, .85) + bar(rr - 36, y, 14, q.muted, .6) + bar(rr - 18, y, 10, q.muted, .6))
        out.append(f'<symbol id="mini-{name}" viewBox="0 0 148 86"><g clip-path="url(#mini-clip)">{"".join(s)}</g>'
                   f'<rect x=".5" y=".5" width="147" height="85" rx="5.5" fill="none" stroke="{q.line}"/></symbol>')
    return ''.join(out)


def ring_at(p, i):
    x, y = cell(i)
    cx, cy = x + 148, y
    return (rect(x - 4, y - 4, 156, 94, None, 12, p.accent) + rect(x - 3, y - 3, 154, 92, None, 11, p.accent)
            + f'<circle cx="{num(cx)}" cy="{num(cy)}" r="9" fill="{p.accent}" stroke="{p.card}" stroke-width="2"/>'
            + f'<path d="M{num(cx - 4)} {num(cy)}l2.6 2.6 5-5" fill="none" stroke="{ink(p.accent)}" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>')


def hover_card(p, i, span):
    x, y = cell(i)
    return A.show(rect(x - 4, y - 4, 156, 94, None, 12, p.muted) + text(x + 74, y + 99, list(THEMES)[i], p.text, anchor='middle'), [span])


def dialog(p, sel, ring, extra=''):
    o = [rect(0, 0, DLG_W, DLG_H, p.card, 8, mix(p.line, p.muted, .35)), text(24, 34, 'Theme', p.text, size=17, w=600),
         text(24, 76, 'Dark', p.muted, w=600), text(24, 366, 'Light', p.muted, w=600)]
    for i, name in enumerate(THEMES):
        x, y = cell(i)
        o.append(use('mini-' + name, x, y, 148, 86) + text(x + 74, y + 99, name, p.text if i == sel else p.muted, w=600 if i == sel else 400, anchor='middle'))
    o.append(g(ring_at(p, 0), cls=ring) + hline(0, DLG_W, 646, p.line))
    o.append(rect(434, 666, 104, 34, p.card, 6, p.line) + text(486, 683, 'Cancel', p.text, anchor='middle'))
    o.append(rect(548, 666, 104, 34, p.accent, 6) + text(600, 683, 'Apply', ink(p.accent), w=600, anchor='middle') + extra)
    return ''.join(o)


# -- the story
WX, WY = 40, 56                    # the main window on the card
G = Geo(1120, 660)
W2X, W2Y = 500, 340                # the second window
G2 = Geo(640, 349, pane=0)
DX, DY = 316, 97                   # the theme picker


def C(x, y):
    """A point of the main window's client area, on the card."""
    return WX + x, WY + TITLE_H + y


def D(x, y):
    """A point of the theme picker, on the card."""
    return DX + DLG_K * x, DY + DLG_K * y


class Story:
    def __init__(self, s):
        self.s, self.snaps = dict(s), [(0.0, dict(s))]

    def at(self, t, **kw):
        assert t >= self.snaps[-1][0], (t, self.snaps[-1][0])
        self.s.update(kw)
        self.snaps.append((t, dict(self.s)))

    def state(self, t):
        return [st for tt, st in self.snaps if tt <= t][-1]


class Cursor:
    def __init__(self, x, y):
        self.pts, self.clicks, self.shapes, self.hidden, self.drags = [(0.0, x, y)], [], [(0.0, 'arrow')], [], []

    def to(self, t0, t1, x, y):
        _, px, py = self.pts[-1]
        self.pts += [(t0, px, py), (t1, x, y)]

    def click(self, t):
        self.clicks.append((t,) + self.pts[-1][1:])

    def drag(self, t0, t1, label):
        """Pressed at t0, `label`'s chip in hand from t0 + .1, let go at t1 where the last move ended."""
        self.drags.append((t0, t1, label) + self.pts[-1][1:])

    def shape(self, t, s):
        self.shapes.append((t, s))

    def hide(self, a, b, fo=0, fi=0):
        self.hidden.append((a, b, fo, fi))


def count(n):
    return f'{n:,} item' + ('' if n == 1 else 's')


def selected(rows, i):
    r = rows[i]
    size = f' | Size: {r["size"]}' if r['size'] else ''
    return f'{count(len(rows))}{size} | Date Modified: {r["date"]} | Path: {r["dir"].rstrip(chr(92))}\\{r["name"]}'


def name_search(pool, q):
    words = q.lower().split()
    return by_name([r for r in pool if all(t in r['name'].lower() for t in words)]), tuple(q.split())


S0 = dict(tab_labels=('All', 'Projects'), tab_icons=('folder', 'folder'), tab=0, ghost=(), tab_hover=None, q='', q_sel=False, focus=False,
          on=(), hover_toggle=None, side='All', side_hover=None, rows=EVERYTHING, terms=(), view='list', tile=72, sort=(0, 'asc'),
          sel=None, hover=None, status='2,541,861 items', mode='List', ctl_hover=None, pv=None, menu=None, menubar_hover=None,
          panes=None, focus_pane=0)
REST = C(930, 380)
CAPTIONS = [(0.0, 1.9, 'Instant search', 'results in about 12 ms, across 2.5 million files'),
            (1.9, 7.9, 'Preview anything', 'spreadsheets, audio, PDF, video, pictures, code'),
            (7.9, 12.05, 'Search inside files', 'no file name says it, the contents do'),
            (12.05, 16.7, 'Search by metadata', 'width:>=3840 finds every 4K screenshot and clip'),
            (16.7, 20.7, 'Your view', 'list, grid, or Windows-style per folder'),
            (20.7, 23.3, 'Tabs', 'merge every open window into one'),
            (23.3, 26.9, 'Split tabs', 'drop a tab onto another: several folders, one tab'),
            (26.9, 31.3, '16 themes', 'switched live, with a cross-fade')]
END = (31.35, 35.7)  # the end card, over the dimmed window; the loop resets under it


def make_story():
    s, c, keys = Story(S0), Cursor(*REST), []

    def retab(label, icon='folder'):
        i = s.s['tab']
        return dict(tab_labels=s.s['tab_labels'][:i] + (label,) + s.s['tab_labels'][i + 1:],
                    tab_icons=s.s['tab_icons'][:i] + (icon,) + s.s['tab_icons'][i + 1:])

    # 1. instant search
    c.to(0.08, 0.5, *C(165, 57.5))
    c.shape(0.4, 'ibeam')
    c.click(0.55)
    s.at(0.55, focus=True)
    word = 'elden'
    for k in range(len(word)):
        q = word[:k + 1]
        rows, terms = name_search(POOL, q)
        s.at(0.8 + .12 * k, q=q, rows=rows, terms=terms, status={'e': '1,318,204 items', 'el': '96,412 items'}.get(q, count(len(rows))), **retab(q))
    c.hide(0.8, 1.75)

    # 2. preview anything
    rows = s.s['rows']
    c.to(1.75, 2.1, *C(300, 187))
    c.shape(1.8, 'arrow')
    s.at(2.02, hover=2)
    c.click(2.22)
    s.at(2.22, sel=2, focus=False, pv=preview(rows[2]), status=selected(rows, 2))
    c.to(2.45, 2.95, *C(1050, 98))
    s.at(2.5, hover=None)
    downs = [3.15, 4.0, 4.85, 6.45, 7.3]
    for k, t in enumerate(downs):
        s.at(t, sel=3 + k, pv=preview(rows[3 + k], t, downs[k + 1] if k + 1 < len(downs) else 7.95), status=selected(rows, 3 + k))
        keys.append((t, ('↓',)))

    # 3. search inside files: nothing is named that, Content finds it
    c.to(7.95, 8.38, *C(235, 57.5))
    c.shape(8.28, 'ibeam')
    c.click(8.48)
    s.at(8.48, focus=True)
    s.at(8.66, q_sel=True)
    keys.append((8.66, ('Ctrl', 'A')))
    phrase = 'nether portal'
    for k in range(len(phrase)):
        q = phrase[:k + 1]
        rows, terms = name_search(QPOOL, q)
        s.at(8.85 + .055 * k, q=q, q_sel=False, rows=rows, terms=terms, sel=None, pv=None, status=count(len(rows)), **retab(q))
    c.hide(8.85, 9.85)
    c.to(9.85, 10.28, *C(921, 57.5))
    c.shape(9.92, 'arrow')
    s.at(10.2, hover_toggle='Content')
    c.click(10.4)
    s.at(10.4, on=('Content',), rows=CONTENT, terms=(), status=count(len(CONTENT)))
    c.to(10.62, 11.0, *C(285, 187))
    s.at(10.66, hover_toggle=None)
    s.at(10.93, hover=2)
    c.click(11.12)
    s.at(11.12, sel=2, focus=False, pv=preview(CONTENT[2]), status=selected(CONTENT, 2))
    c.to(11.35, 11.75, *C(640, 520))
    s.at(11.4, hover=None)

    # 4. search by metadata, then the grid
    c.to(12.1, 12.5, *C(921, 57.5))
    c.click(12.58)
    s.at(12.58, on=(), rows=[], sel=None, pv=None, status=count(0), hover_toggle='Content')
    c.to(12.66, 12.88, *C(845, 57.5))
    s.at(12.74, hover_toggle='Metadata')
    c.click(12.95)
    s.at(12.95, on=('Metadata',), hover_toggle=None, focus=True)
    s.at(13.1, q_sel=True)
    keys.append((13.1, ('Ctrl', 'A')))
    meta = 'width:>=3840'
    for k in range(len(meta)):
        q = meta[:k + 1]
        if q == meta:
            rows, terms, st = HIRES, (), count(len(HIRES))
        elif q.startswith('width:'):
            rows, terms, st = MEDIA, (), '79,560 items' if q.endswith('384') else '84,113 items'
        else:
            rows, terms = name_search(MEDIA, q)
            st = count(len(rows))
        s.at(13.26 + .05 * k, q=q, q_sel=False, rows=rows, terms=terms, status=st, **retab(q))
    c.hide(13.26, 14.05)
    c.to(14.05, 14.48, *C(sum(G.ctl['Grid']) / 2, G.bar + 14.5))
    s.at(14.4, ctl_hover='Grid')
    c.click(14.58)
    s.at(14.58, view='grid', mode='Grid', ctl_hover=None)
    sx, sy, cols = G.grid(72)
    c.to(14.78, 15.2, *C(G.side + 4 * sx + sx / 2, 97 + 36))
    s.at(15.12, hover=4)
    c.click(15.32)
    s.at(15.32, sel=4, focus=False, pv=preview(HIRES[4], 15.32, 18.05), status=selected(HIRES, 4))
    c.to(15.55, 15.95, *C(560, 430))
    s.at(15.6, hover=None)

    # 5. views
    c.to(16.75, 17.12, *C(sum(G.ctl['Windows']) / 2, G.bar + 14.5))
    s.at(17.05, ctl_hover='Windows')
    c.click(17.25)
    s.at(17.25, mode='Windows', view='list', ctl_hover=None)
    c.to(17.45, 17.92, *C(100, G.rows['Pictures'] + 13.5))
    s.at(17.85, side_hover='Pictures')
    c.click(18.05)
    s.at(18.05, side='Pictures', side_hover=None, q='child:"D:\\Pictures"', terms=(), rows=PICTURES, view='grid', tile=96,
         sel=None, pv=None, status=count(len(PICTURES)), **retab('Pictures', 'pictures'))
    c.to(18.85, 19.3, *C(100, G.rows['Documents'] + 13.5))
    s.at(19.22, side_hover='Documents')
    c.click(19.42)
    s.at(19.42, side='Documents', side_hover=None, q='child:"D:\\Documents"', rows=DOCUMENTS, view='list', sort=(4, 'desc'),
         status=count(len(DOCUMENTS)), **retab('Documents', 'documents'))
    c.to(19.62, 20.05, *C(560, 480))

    # 6. a second window, merged into this one
    x0, x1 = G.menus['Tabs']
    c.to(20.95, 21.42, *C((x0 + x1) / 2, 18.5))
    s.at(21.36, menubar_hover='Tabs')
    c.click(21.55)
    s.at(21.55, menu=('Tabs', None), menubar_hover=None)
    _, mrows, mw = menu(Pal('Midnight'), x0, MENU_Y, TABS_MENU)
    win = mrows[10]
    c.to(21.68, 22.0, *C(x0 + 62, sum(win) / 2))
    s.at(21.95, menu=('Tabs', 10))
    s.at(22.12, menu=('Tabs', 10, None))
    _, srows, _ = menu(Pal('Midnight'), x0 + mw, win[0] - 3, WINDOWS_MENU)
    c.to(22.18, 22.45, *C(x0 + mw + 76, sum(srows[2]) / 2))
    s.at(22.4, menu=('Tabs', 10, 2))
    c.click(22.6)
    s.at(22.62, menu=None)
    s.at(22.7, tab_labels=s.s['tab_labels'] + ('Downloads', 'Music'), tab_icons=s.s['tab_icons'] + ('downloads', 'music'), ghost=(2, 3))
    s.at(23.18, ghost=(3,))
    s.at(23.24, ghost=())
    # 7. split tabs: Downloads dropped on this tab's right half, Music under it
    xs = G.tabs(s.s['tab_labels'])
    c.to(23.3, 23.66, *C(sum(xs[2]) / 2, 21))
    s.at(23.58, tab_hover=2)
    c.to(23.78, 24.45, *C(600, 280))  # the chip clear of the zone's label
    c.drag(23.76, 24.62, 'Downloads')
    s.at(23.86, tab_hover=None, ghost=(2,))  # past the drag threshold: the chip takes it
    s.at(24.62, tab_labels=(('Documents', 'Downloads'), 'Projects', 'Music'), tab_icons=('downloads', 'folder', 'music'), ghost=(),
         panes=(('Documents', DOCUMENTS, (4, 'desc')), ('Downloads', DOWNLOADS, (4, 'desc'))), focus_pane=1,
         side='Downloads', q='child:"D:\\Downloads"', status=count(len(DOWNLOADS)))
    xs = G.tabs(s.s['tab_labels'])
    c.to(24.8, 25.18, *C(sum(xs[2]) / 2, 21))
    s.at(25.1, tab_hover=2)
    c.to(25.3, 25.68, *C(600, 160))
    c.to(25.74, 26.0, *C(600, 425))
    c.drag(25.28, 26.12, 'Music')
    s.at(25.38, tab_hover=None, ghost=(2,))
    s.at(26.12, tab_labels=(('Documents', 'Downloads', 'Music'), 'Projects'), tab_icons=('music', 'folder'), ghost=(),
         panes=s.s['panes'] + (('Music', MUSIC, (0, 'asc')),), focus_pane=2, side='Music', q='child:"D:\\Media\\Music"', status=count(len(MUSIC)))

    # 8. themes
    v0, v1 = G.menus['View']
    c.to(26.95, 27.38, *C((v0 + v1) / 2, 18.5))
    s.at(27.32, menubar_hover='View')
    c.click(27.5)
    s.at(27.5, menu=('View', None), menubar_hover=None)
    _, vrows, _ = menu(Pal('Midnight'), v0, MENU_Y, VIEW_MENU)
    c.to(27.6, 27.9, *C(v0 + 52, sum(vrows[10]) / 2))
    s.at(27.86, menu=('View', 10))
    c.click(28.0)
    s.at(28.02, menu=None)
    for t0, t1, i in [(28.25, 28.65, 8), (29.3, 29.75, 3)]:
        x, y = cell(i)
        c.to(t0, t1, *D(x + 80, y + 46))
    c.clicks += [(28.8,) + D(cell(8)[0] + 80, cell(8)[1] + 46)]
    c.clicks += [(29.92,) + D(cell(3)[0] + 80, cell(3)[1] + 46)]
    c.to(30.4, 30.85, *D(490, 684))
    c.click(31.05)
    c.hide(31.35, 36.0, .2, .25)  # the end card
    c.to(32.0, 32.4, *REST)
    c.clicks.sort()

    s.at(RESET, **S0)
    return s, c, keys


# -- the card around it all
def card_bg():
    return (lg('card-bg', [(0, '#121722'), (1, '#0a0d12')]) + rg('glow-a', [(0, '#7aa2ff', .2), (1, '#7aa2ff', 0)], 220, 40, 620, ' gradientUnits="userSpaceOnUse"')
            + rg('glow-b', [(0, '#b07bff', .13), (1, '#b07bff', 0)], 1040, 800, 560, ' gradientUnits="userSpaceOnUse"'))


def top_band(cpu_pts, ram_pts):
    o = [use('i-logo', 38, 11, 34), text(80, 28, 'Mosaic', '#f0f3f7', size=21, w=600),
         text(80 + tw('Mosaic', 21, 600) + 14, 29, 'Instant file search and a fast explorer for Windows', '#8b93a0', size=13)]
    # the resource meter: what Mosaic itself uses, as Task Manager shows it
    files = '2,541,861 files indexed'
    wf, wr, wc = tw(files), tw('RAM') + 6 + tw('65 MB'), tw('CPU') + 6 + tw('0%')
    width = 14 + 16 + wf + 25 + wr + 25 + wc + 14
    x, cy = 1160 - width, 28
    o.append(rect(x, cy - 14, width, 28, '#ffffff', 14, op=.045) + rect(x, cy - 14, width, 28, None, 14, '#ffffff', op=.09))
    x += 14
    o.append(f'<g transform="translate({num(x + 4)} {cy})"><circle class="pulse" r="3.5" fill="#3fb950"/><circle r="3.5" fill="#3fb950"/></g>')
    x += 16
    o.append(text(x, cy, files, '#c9d1d9'))
    x += wf + 12
    o.append(vline(x, cy - 7, cy + 7, '#ffffff26'))
    x += 13
    o.append(text(x, cy, 'RAM', '#8b93a0'))
    vx = x + tw('RAM') + 6
    o.append(stepped(ram_pts, lambda v: text(vx, cy, f'{v} MB', '#e6e8eb', w=600)))
    x += wr + 12
    o.append(vline(x, cy - 7, cy + 7, '#ffffff26'))
    x += 13
    o.append(text(x, cy, 'CPU', '#8b93a0'))
    cx = x + tw('CPU') + 6
    o.append(stepped(cpu_pts, lambda v: text(cx, cy, f'{v}%', '#e6e8eb', w=600)))
    return ''.join(o)


def captions():
    o, cy = [], 778
    for k, (a, b, title, sub) in enumerate(CAPTIONS):
        line = text(600, cy, f'<tspan font-weight="600" fill="#e6e8eb">{esc(title)}</tspan><tspan fill="#8b93a0">  ·  {esc(sub)}</tspan>', '#e6e8eb', size=15, anchor='middle', raw=True)
        spans = [(a, b, .3, .2)] if a else [(0, b, 0, .2), (END[1] + .1, T, .3, 0)]
        o.append(A.show(line, spans))
    n, wd, gap = len(CAPTIONS), 40, 6
    x0 = 600 - (n * wd + (n - 1) * gap) / 2
    for k, (a, b, _, _) in enumerate(CAPTIONS):
        x = x0 + k * (wd + gap)
        z = END[1]  # full till the end card goes
        fill = A.track('transform:scaleX({});opacity:{}', [(a, (0, 1)), (b, (1, 1)), (z, (1, 1)), (z + .3, (1, 0)), (z + .3 + E, (0, 1))], 'linear')
        o.append(rect(x, 799, wd, 3, '#ffffff', 1.5, op=.1) + f'<g transform="translate({num(x)} 799)"><rect class="{fill}" width="{wd}" height="3" rx="1.5" fill="#7aa2ff"/></g>')
    return ''.join(o)


def end_card():
    a, b = END
    veil = A.show(rect(WX, WY, G.w, G.h + TITLE_H, '#0b0e14', 8, op=.9), [(a - .1, b + .15, .35, .4)])
    cx, cy = WX + G.w / 2, WY + (G.h + TITLE_H) / 2
    first, second = 'Every file on your PC, ', 'before you finish typing.'
    w1, w2 = tw(first, 34, 600), tw(second, 34, 600)
    x0 = cx - (w1 + w2) / 2
    grad = lg('end-ink', [(0, '#8fb3ff'), (1, '#c39bff')], x0 + w1, 0, x0 + w1 + w2, 0, ' gradientUnits="userSpaceOnUse"')
    rise = A.track('transform:translateY({}px)', [(a, 12), (a + .5, 0), (b + .3, 0), (b + .3 + E, 12)], 'cubic-bezier(.2,.7,.3,1)')
    inner = (f'<ellipse cx="{num(cx)}" cy="{num(cy)}" rx="420" ry="150" fill="url(#end-glow)"/>' + use('i-logo', cx - 34, cy - 118, 68)
             + text(x0, cy - 6, f'{esc(first)}<tspan fill="url(#end-ink)">{esc(second)}</tspan>', '#f0f3f7', size=34, w=600, raw=True)
             + text(cx, cy + 42, 'Opens in 348 ms  ·  51–65 MB of RAM  ·  a 9.5 MB app  ·  MIT licensed', '#8b93a0', size=15, anchor='middle'))
    return grad + rg('end-glow', [(0, '#7aa2ff', .16), (1, '#7aa2ff', 0)]), veil + A.show(f'<g class="{rise}">{inner}</g>', [(a + .1, b, .4, .25)])


def keycaps(keys):
    o = []
    for t, caps in keys:
        x, parts = 1160, []
        for label in reversed(caps):
            w = max(28, tw(label, 12, 600) + 18)
            x -= w
            parts.append(rect(x, 765, w, 27, '#0b0e13', 6) + rect(x, 763, w, 27, '#1b2029', 6, '#353c47') + text(x + w / 2, 776.5, label, '#e6e8eb', w=600, anchor='middle'))
            x -= 6
        o.append(A.show(''.join(parts), [(t - .03, t + .55, .06, .22)]))
    return ''.join(o)


ARROW = ('<path d="M0 0V17.2l4.3-4.1 2.9 6.4 2.9-1.3-2.8-6.2h5.9z" fill="#000" opacity=".28" transform="translate(1 1.6)"/>'
         '<path d="M0 0V17.2l4.3-4.1 2.9 6.4 2.9-1.3-2.8-6.2h5.9z" fill="#fff" stroke="#000" stroke-linejoin="round"/>')
IBEAM = ''.join(f'<path d="M-3.5-8.5h1.6c.9 0 1.6.5 1.9 1.1.3-.6 1-1.1 1.9-1.1h1.6M0-7.2v14.4M-3.5 8.5h1.6c.9 0 1.6-.5 1.9-1.1.3.6 1 1.1 1.9 1.1h1.6" fill="none" stroke="{c}" stroke-width="{w}"/>'
                for c, w in (('#fff', 3), ('#000', 1.1)))


def cursor(c):
    shown, t, f = [], 0.0, 0
    for a, b, fo, fi in sorted(c.hidden):
        shown.append((t, a, f, fo))
        t, f = b, fi
    shown.append((t, T, f, 0))
    X = A.track('transform:translateX({}px)', [(t, x) for t, x, _ in c.pts], 'cubic-bezier(.3,.1,.2,1)')
    Y = A.track('transform:translateY({}px)', [(t, y) for t, _, y in c.pts], 'cubic-bezier(.55,0,.3,1)')
    press, chips, M = [], [], Pal('Midnight')
    for t, _, _ in c.clicks:
        press += [(t - .06, 1), (t, .82), (t + .16, 1)]
    for t0, t1, label, _, _ in c.drags:  # held down; the chip (native.rs paint_chip) rides 14, 12 off the point
        press += [(t0 - .06, 1), (t0, .84), (t1, .84), (t1 + .16, 1)]
        w = round(tw(label)) + 28
        chips.append(A.show(shadow(14, 12, w, 31, 8, .35, 3) + rect(14, 12, w, 31, M.hl, 6, M.accent) + text(28, 27.5, label, M.text), [(t0 + .1, t1, .08, .08)]))
    P = A.track('transform:scale({})', press)
    sh = sorted(c.shapes)
    spans = {'arrow': [], 'ibeam': []}
    for k, (t, s) in enumerate(sh):
        spans[s].append((t, sh[k + 1][0] if k + 1 < len(sh) else T, 0, 0))
    shapes = A.show(g(ARROW, 'scale(1.15)'), spans['arrow']) + A.show(g(IBEAM, 'scale(1.15)'), spans['ibeam'])
    ripples = []
    for t, x, y in c.clicks + [d[1:2] + d[3:] for d in c.drags]:  # a drop ripples too
        R = A.track('transform:scale({});opacity:{}', [(t, (.3, 0)), (t + E, (.3, .9)), (t + .45, (1.3, 0))], 'cubic-bezier(.2,.7,.3,1)')
        ripples.append(f'<g transform="translate({num(x)} {num(y)})"><g class="{R}"><circle r="15" fill="#7aa2ff" opacity=".22"/><circle r="15" fill="none" stroke="#7aa2ff" stroke-width="2"/></g></g>')
    return ''.join(ripples) + A.show(f'<g class="{X}"><g class="{Y}">{"".join(chips)}<g class="{P}">{shapes}</g></g></g>', shown)


def window_static(p, G, st):
    return ''.join([piece_frame(p, G), piece_title(p, G, st), piece_strip(p, G, st), piece_search(p, G, st), piece_side(p, G, st),
                    piece_marks(p, G, st), piece_list(p, G, st), piece_pane(p, G, st), piece_status(p, G, st)])


def build():
    M = Pal('Midnight')
    story, cur, keys = make_story()
    snaps = story.snaps

    def layer(fn, fade=(0, 0), fx=None):
        return timeline(snaps, lambda st: fn(M, G, st), fade, fx)

    body = [f'<rect width="{CW}" height="{CH}" rx="20" fill="url(#card-bg)"/><rect width="{CW}" height="{CH}" rx="20" fill="url(#glow-a)"/>'
            f'<rect width="{CW}" height="{CH}" rx="20" fill="url(#glow-b)"/><rect x=".5" y=".5" width="{CW - 1}" height="{CH - 1}" rx="19.5" fill="none" stroke="#ffffff" stroke-opacity=".08"/>']
    ram = [(0, 51), (2.22, 53), (3.15, 55), (4.85, 58), (6.45, 56), (10.4, 57), (13.81, 59), (14.58, 62), (15.32, 64), (18.05, 63), (19.42, 60),
           (20.75, 62), (22.7, 61), (24.62, 62), (26.12, 63), (28.05, 64), (31.1, 62), (RESET, 51)]
    cpu = [(0, 0), (0.8, 1), (1.6, 0), (2.22, 1), (2.6, 0), (4.85, 1), (6.45, 0), (8.85, 1), (9.8, 0), (10.4, 2), (10.7, 0), (13.26, 1), (14.0, 0),
           (14.58, 2), (14.9, 0), (15.32, 1), (17.25, 0), (18.05, 1), (18.4, 0), (22.7, 1), (23.1, 0), (24.62, 1), (24.9, 0), (26.12, 1), (26.4, 0),
           (28.8, 1), (29.1, 0), (29.92, 1), (30.2, 0)]
    body.append(top_band(cpu, ram))

    drops = {24.62: (.15, .15), 26.12: (.15, .15)}
    win = [piece_frame(M, G), layer(piece_title), layer(piece_strip, fx={22.7: (.15, .15), 23.18: (0, .12), 23.24: (0, .12), 23.86: (.08, .08), 25.38: (.08, .08), **drops}),
           layer(piece_search), layer(piece_side), layer(piece_marks), layer(piece_list, fx=drops), layer(piece_pane, (.08, .14)), layer(piece_status)]
    body.append(shadow(WX, WY, G.w, G.h + TITLE_H, 46, .55, 22) + g(''.join(win), f'translate({WX} {WY})'))

    # where a dragged tab will dock (native.rs paint_ov): it grows in with a spring, and follows the pointer from
    # the top half of the Downloads pane to the bottom one
    spring = 'cubic-bezier(.3,1.45,.5,1)'
    for (l, t, r, b), label, (a, z), dy in [(pane_rects(G, 2)[1], 'Downloads', (24.1, 24.62), None),
                                            (pane_rects(G, 3)[1], 'Music', (25.5, 26.12), pane_rects(G, 3)[2][1] - pane_rects(G, 3)[1][1])]:
        rad, sc = min((b - t) / 4, 10), A.track('transform:scale({})', [(a, .94), (a + .32, 1), (z + .2, 1)], spring)
        zone = (rect(l, t, r - l, b - t, M.accent, rad, op=.16) + rect(l, t, r - l, b - t, None, rad, M.accent, op=.9)
                + rect(l + 1, t + 1, r - l - 2, b - t - 2, None, rad - 1, M.accent, op=.5) + text((l + r) / 2, (t + b) / 2, label, M.accent, anchor='middle'))
        zone = about(WX + (l + r) / 2, WY + TITLE_H + (t + b) / 2, sc, g(zone, f'translate({WX} {WY + TITLE_H})'))
        if dy:
            zone = g(zone, cls=A.track('transform:translateY({}px)', [(a, 0), (25.8, 0), (26.0, dy), (z + .2, dy)], 'cubic-bezier(.3,0,.2,1)'))
        body.append(A.show(zone, [(a, z, .12, .1)]))

    # the second window: opens, then gives its tabs away
    S2 = dict(S0, tab_labels=('Downloads', 'Music'), tab_icons=('downloads', 'music'), q='child:"D:\\Downloads"', side='Downloads',
              rows=DOWNLOADS, sort=(4, 'desc'), status=count(len(DOWNLOADS)), mode='Windows')
    w2 = (piece_frame(M, G2) + piece_title(M, G2, S2) + A.show(piece_strip(M, G2, S2), [(20, 22.7, 0, 0)])
          + A.show(piece_strip(M, G2, dict(S2, ghost=(0, 1))), [(22.7, 24, 0, 0)]) + piece_search(M, G2, S2) + piece_side(M, G2, S2)
          + piece_list(M, G2, S2) + piece_status(M, G2, S2))
    w2 = shadow(0, 0, G2.w, G2.h + TITLE_H, 40, .5, 18) + w2
    sc = A.track('transform:scale({})', [(20.75, .965), (21.05, 1), (22.72, 1), (22.92, .97)])
    body.append(A.show(about(W2X + G2.w / 2, W2Y + G2.h / 2, sc, g(w2, f'translate({W2X} {W2Y})')), [(20.75, 22.72, .25, .2)]))

    # its tabs, flying into the main strip
    src, dst = G2.tabs(('Downloads', 'Music')), G.tabs(('Documents', 'Projects', 'Downloads', 'Music'))
    for i, (lab, ic) in enumerate([('Downloads', 'downloads'), ('Music', 'music')]):
        t0, t1 = 22.7 + .06 * i, 23.18 + .06 * i
        cw = 150
        chip = (shadow(-cw / 2, -14.5, cw, 29, 14, .4, 6) + rect(-cw / 2, -14.5, cw, 29, M.card, 8, M.accent if i == 0 else mix(M.line, M.muted, .4))
                + use('i-' + ic, -cw / 2 + 12, -8, 16) + text(-cw / 2 + 34, 0, lab, M.text))
        a = (W2X + sum(src[i]) / 2, W2Y + TITLE_H + 20.5)
        b = (WX + sum(dst[2 + i]) / 2, WY + TITLE_H + 20.5)
        mv = A.track('transform:translate({}px,{}px)', [(t0, a), (t1, b), (t1 + .3, b)], 'cubic-bezier(.5,0,.2,1)')
        lift = A.track('transform:scale({})', [(t0, 1), ((t0 + t1) / 2, 1.07), (t1, 1)])
        body.append(A.show(f'<g class="{mv}"><g class="{lift}">{chip}</g></g>', [(t0, t1, 0, .12)]))

    body.append(g(layer(piece_menu), f'translate({WX} {WY})'))

    # the theme picker, and the window in the themes picked
    d = lambda i: (cell(i)[0] - cell(0)[0], cell(i)[1] - cell(0)[1])
    ring = A.track('transform:translate({}px,{}px)', [(28.8, (0, 0)), (29.05, d(8)), (29.92, d(8)), (30.17, d(3)), (31.5, d(3))], 'cubic-bezier(.3,0,.2,1)')
    dshadow = shadow(DX, DY, DLG_W * DLG_K, DLG_H * DLG_K, 40, .5, 16)

    def picker(p, sel, extra):
        return dshadow + g(dialog(p, sel, ring, extra), f'translate({DX} {DY}) scale({DLG_K})')
    opening = A.track('transform:scale({})', [(28.05, .965), (28.25, 1), (29.1, 1)])
    body.append(A.show(about(DX + DLG_W * DLG_K / 2, DY + DLG_H * DLG_K / 2, opening, picker(M, 0, hover_card(M, 8, (28.6, 28.8, 0, 0)))), [(28.05, 29.06, .16, 0)]))
    theme_state = story.state(28.5)
    for name, (a, b, fo), sel, extra in [
            ('Daylight', (28.8, 30.16, 0), 8, lambda q: hover_card(q, 3, (29.7, 29.92, 0, 0))),
            ('Ocean', (29.92, 31.05, .3), 3, lambda q: A.show(rect(434, 666, 104, 34, q.hl, 6, q.line) + text(486, 683, 'Cancel', q.text, anchor='middle'), [(30.8, 31.05, 0, 0)]))]:
        q = Pal(name)  # Cancel: the picker goes at once, the window fades back to Midnight
        body.append(A.show(g(window_static(q, G, theme_state), f'translate({WX} {WY})'), [(a, b, .22, fo)]) + A.show(picker(q, sel, extra(q)), [(a, b, .22, min(fo, .08))]))

    end_defs, end = end_card()
    body.append(end + captions() + keycaps(keys) + cursor(cur))

    defs = (gradients() + shadow_defs() + card_bg() + end_defs + icon_defs() + scenes() + mini_defs()
            + ''.join(f'<clipPath id="clip-list-{q.w}"><rect x="{q.ll - 1}" y="87" width="{q.lr - q.ll + 2}" height="{q.bar - 87}"/></clipPath>' for q in (G, G2)))
    css = (f'.m{{font-family:{MONO}}}.caret{{animation:blink 1.06s step-end infinite}}@keyframes blink{{50%{{opacity:0}}}}'
           '.pulse{animation:pulse 2.4s cubic-bezier(.2,.6,.3,1) infinite}@keyframes pulse{0%{transform:scale(1);opacity:.55}100%{transform:scale(3.2);opacity:0}}'
           + A.css() + '@media (prefers-reduced-motion:reduce){*{animation:none!important}}')
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {CW} {CH}" width="{CW}" height="{CH}" font-family="{SANS}" font-size="12" role="img" '
            f'aria-label="Mosaic, a file explorer and instant search for Windows: searching 2.5 million files as you type, previews, content and metadata search, views, tab merging, split tabs and themes">'
            f'<title>Mosaic: every file on your PC, before you finish typing</title><style>{css}</style><defs>{defs}</defs>{"".join(body)}</svg>')


def css_rules(self):
    return ''.join(self.rules)


Anim.css = css_rules

if __name__ == '__main__':
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'docs', 'hero.svg')
    svg = build()
    with open(out, 'w', encoding='utf-8', newline='\n') as f:
        f.write(svg)
    print(f'{out}: {len(svg.encode()) / 1024:.0f} KB, {len(A.memo)} animations')
