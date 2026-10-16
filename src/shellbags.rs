//! Explorer's own memory of each folder's view ("ShellBags", under `BASE` in HKCU), read and written so a folder
//! looks the same in Mosaic and in Explorer.
//!
//! `BagMRU` is a tree mirroring the folders' item ID lists (Desktop at its root): each node's numbered values are
//! its children's item IDs, its numbered subkeys their nodes, `MRUListEx` their order (newest first), `NodeSlot`
//! the bag holding its view. The root's `NodeSlots` has a byte per slot taken. `Bags\<slot>\Shell\{folder type}`
//! holds the view: `Mode`, `LogicalViewMode`, `IconSize`, `Vid`, `Sort`.
//!
//! Reading and updating a bag are plain registry calls, here. Adding a folder Explorer never saw needs its real
//! item IDs, which only the shell makes: [`create`] runs in a helper process (`--bag-write`), as the shell loads
//! third-party folder extensions into whoever asks.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Registry::*;

pub const BASE: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\Shell";

const MY_COMPUTER: [u8; 16] = guid(0x20D04FE0, 0x3AEA, 0x1069, [0xA2, 0xD8, 0x08, 0x00, 0x2B, 0x30, 0x30, 0x9D]);
const USERS_FILES: [u8; 16] = guid(0x59031a47, 0x3f72, 0x44a7, [0x89, 0xc5, 0x55, 0x95, 0xfe, 0x6b, 0x30, 0xee]);
const HOME: [u8; 16] = guid(0xF874310E, 0xB6B7, 0x47DC, [0xBC, 0x84, 0xB9, 0xE6, 0xB3, 0x8F, 0x59, 0x03]);
/// The folders This PC and Home show at their top, each by the two ids This PC has had for it.
const KNOWN: [[[u8; 16]; 2]; 6] = [
    [guid(0xB4BFCC3A, 0xDB2C, 0x424C, [0xB0, 0x29, 0x7F, 0xE9, 0x9A, 0x87, 0xC6, 0x41]), guid(0x754AC886, 0xDF64, 0x4CBA, [0x86, 0xB5, 0xF7, 0xFB, 0xF4, 0xFB, 0xCE, 0xF5])], // Desktop
    [guid(0xA8CDFF1C, 0x4878, 0x43BE, [0xB5, 0xFD, 0xF8, 0x09, 0x1C, 0x1C, 0x60, 0xD0]), guid(0xD3162B92, 0x9365, 0x467A, [0x95, 0x6B, 0x92, 0x70, 0x3A, 0xCA, 0x08, 0xAF])], // Documents
    [guid(0x374DE290, 0x123F, 0x4565, [0x91, 0x64, 0x39, 0xC4, 0x92, 0x5E, 0x46, 0x7B]), guid(0x088E3905, 0x0323, 0x4B02, [0x98, 0x26, 0x5D, 0x99, 0x42, 0x8E, 0x11, 0x5F])], // Downloads
    [guid(0x1CF1260C, 0x4DD0, 0x4EBB, [0x81, 0x1F, 0x33, 0xC5, 0x72, 0x69, 0x9F, 0xDE]), guid(0x3DFDF296, 0xDBEC, 0x4FB4, [0x81, 0xD1, 0x6A, 0x34, 0x38, 0xBC, 0xF4, 0xDE])], // Music
    [guid(0x3ADD1653, 0xEB32, 0x4CB0, [0xBB, 0xD7, 0xDF, 0xA0, 0xAB, 0xB5, 0xAC, 0xCA]), guid(0x24AD3AD4, 0xA569, 0x4530, [0x98, 0xE1, 0xAB, 0x02, 0xF9, 0x41, 0x7A, 0xA8])], // Pictures
    [guid(0xA0953C92, 0x50DC, 0x43BF, [0xBE, 0x83, 0x37, 0x42, 0xFE, 0xD0, 0x3C, 0x9C]), guid(0xF86FA3AB, 0x70D2, 0x4FC7, [0x9C, 0x99, 0xFC, 0xBF, 0x05, 0x46, 0x7F, 0x3A])], // Videos
];
const STORAGE: [u8; 16] = guid(0xB725F130, 0x47EF, 0x101A, [0xA5, 0xF1, 0x02, 0x60, 0x8C, 0x9E, 0xEB, 0xAC]); // PKEY_Size's set
const ITEM_DATE: [u8; 16] = guid(0xF7DB74B4, 0x4287, 0x4103, [0xAF, 0xBA, 0xF1, 0xB1, 0x3D, 0xCD, 0x75, 0xCF]);
const VID_ICONS: &str = "{0057D0E0-3573-11CF-AE69-08002B2E1262}";
const VID_DETAILS: &str = "{137E7700-3573-11CF-AE69-08002B2E1262}";
/// Explorer's folder types, by the name it notes as `SniffedFolderType`.
pub const TYPES: [(&str, &str); 6] = [
    ("Generic", "{5C4F28B5-F869-4E84-8E60-F11DB97C5CC7}"),
    ("Pictures", "{B3690E58-E961-423B-B687-386EBFD83239}"),
    ("Videos", "{5FA96407-7E77-483C-AC93-691D05850DE8}"),
    ("Music", "{94D6DDCC-4A68-4175-A374-BD584A510B78}"),
    ("Documents", "{7D49D726-3C21-4F05-99AA-FDC2C9474656}"),
    ("Downloads", "{885A186E-A440-4ADA-812B-DB871B942259}"),
];
/// Explorer keeps this many folders at most: past it, nothing is added (Explorer recycles them its own way).
const MAX_SLOTS: usize = 4900;

const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

/// A folder's view as Explorer keeps it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct View {
    pub grid: bool,
    pub icon: u32,                         // Explorer's icon size (16..256)
    pub sort: Option<(&'static str, bool)>, // Mosaic's column, descending
    pub when: u64,                         // when saved: seconds since 1970
}

// -- the registry
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}
fn wz(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
impl Key {
    fn open(root: HKEY, sub: &str, write: bool) -> Option<Key> {
        let mut k = null_mut();
        let access = if write { KEY_READ | KEY_WRITE } else { KEY_READ };
        (unsafe { RegOpenKeyExW(root, wz(sub).as_ptr(), 0, access, &mut k) } == 0).then_some(Key(k))
    }
    fn sub(&self, name: &str, write: bool) -> Option<Key> {
        Key::open(self.0, name, write)
    }
    fn create(&self, name: &str) -> Option<Key> {
        let mut k = null_mut();
        (unsafe { RegCreateKeyExW(self.0, wz(name).as_ptr(), 0, null(), REG_OPTION_NON_VOLATILE, KEY_READ | KEY_WRITE, null(), &mut k, null_mut()) } == 0).then_some(Key(k))
    }
    fn get(&self, name: &str) -> Option<Vec<u8>> {
        let n = wz(name);
        let mut len = 0u32;
        if unsafe { RegQueryValueExW(self.0, n.as_ptr(), null(), null_mut(), null_mut(), &mut len) } != 0 {
            return None;
        }
        let mut v = vec![0u8; len as usize];
        (unsafe { RegQueryValueExW(self.0, n.as_ptr(), null(), null_mut(), v.as_mut_ptr(), &mut len) } == 0).then(|| {
            v.truncate(len as usize);
            v
        })
    }
    fn dword(&self, name: &str) -> Option<u32> {
        self.get(name).filter(|v| v.len() == 4).map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
    }
    fn text(&self, name: &str) -> Option<String> {
        let v = self.get(name)?;
        let w: Vec<u16> = v.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        Some(String::from_utf16_lossy(&w))
    }
    fn set(&self, name: &str, kind: REG_VALUE_TYPE, data: &[u8]) -> bool {
        unsafe { RegSetValueExW(self.0, wz(name).as_ptr(), 0, kind, data.as_ptr(), data.len() as u32) == 0 }
    }
    fn set_dword(&self, name: &str, v: u32) -> bool {
        self.set(name, REG_DWORD, &v.to_le_bytes())
    }
    fn set_text(&self, name: &str, s: &str) -> bool {
        let b: Vec<u8> = wz(s).iter().flat_map(|c| c.to_le_bytes()).collect();
        self.set(name, REG_SZ, &b)
    }
    fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0.. {
            let (mut b, mut n) = ([0u16; 256], 256u32);
            if unsafe { RegEnumKeyExW(self.0, i, b.as_mut_ptr(), &mut n, null(), null_mut(), null_mut(), null_mut()) } != 0 {
                break;
            }
            out.push(String::from_utf16_lossy(&b[..n as usize]));
        }
        out
    }
    /// When the key was last written: seconds since 1970.
    fn written(&self) -> u64 {
        let mut ft = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let n = null_mut();
        unsafe { RegQueryInfoKeyW(self.0, null_mut(), n, null(), n, n, n, n, n, n, n, &mut ft) };
        let t = (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64;
        (t / 10_000_000).saturating_sub(11_644_473_600)
    }
}

// -- item IDs
/// What one item of an item ID list names, as far as matching a path needs.
#[derive(PartialEq, Debug, Clone)]
pub enum Part {
    Clsid([u8; 16]), // a shell folder: This PC, the user's own folder…
    Drive(u8),       // "C:\", upper case
    Name(String),    // a folder, by its long name, lower case
}

/// What item ID `item` (with its size, as stored) names; None for kinds no path goes through.
pub fn part_of(item: &[u8]) -> Option<Part> {
    let (&t, len) = (item.get(2)?, item.len());
    if (t == 0x1F || t == 0x2E) && len >= 20 {
        return Some(Part::Clsid(item[4..20].try_into().ok()?));
    }
    if t == 0x79 {
        // ponytail: only Home's known folders (an item list This PC > folder inside); a pinned folder anywhere else
        // wraps a longer list, not matched
        let i = (0..len.saturating_sub(40)).find(|&i| item[i..i + 3] == [0x14, 0, 0x1F] && item[i + 4..i + 20] == MY_COMPUTER)? + 20;
        let n = u16::from_le_bytes([*item.get(i)?, *item.get(i + 1)?]) as usize;
        let inner = item.get(i..i + n)?;
        return matches!(inner.get(2), Some(0x2E)).then(|| part_of(inner)).flatten();
    }
    if t & 0x70 == 0x20 && len >= 6 && item[4] == b':' && item[3].is_ascii_alphabetic() {
        return Some(Part::Drive(item[3].to_ascii_uppercase()));
    }
    // a file system folder (or a delegate wrapping one): its long name, in its 0xBEEF0004 extension block
    let at = (4..len.saturating_sub(4)).find(|&i| item[i..i + 4] == [0x04, 0x00, 0xEF, 0xBE])?;
    let block = at - 4;
    let size = u16::from_le_bytes([item[block], item[block + 1]]) as usize;
    let off = match u16::from_le_bytes([item[block + 2], item[block + 3]]) {
        9.. => 0x2E,
        8 => 0x2A,
        7 => 0x26,
        3..=6 => 0x14,
        _ => return None,
    };
    let (start, end) = (block + off, (block + size).min(len));
    let w: Vec<u16> = item.get(start..end)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    (!w.is_empty()).then(|| Part::Name(String::from_utf16_lossy(&w).to_lowercase()))
}

/// The ways Explorer may have reached folder `path` (its item ID lists, as parts): through This PC and the drive,
/// through the user's own folder when it's in there, at the Desktop's top when it's on it.
pub fn chains(path: &str, at: &Places) -> Vec<Vec<Part>> {
    let (profile, desktop) = (at.profile.as_str(), at.desktop.as_str());
    let split = |p: &str| -> Vec<String> { p.trim_end_matches('\\').split('\\').filter(|s| !s.is_empty()).map(str::to_lowercase).collect() };
    let p = split(path);
    let mut out = Vec::new();
    let Some(drive) = p.first().filter(|d| d.len() == 2 && d.ends_with(':')).map(|d| d.as_bytes()[0].to_ascii_uppercase()) else { return out };
    let names = |v: &[String]| v.iter().map(|s| Part::Name(s.clone())).collect::<Vec<_>>();
    let under = |base: &str| {
        let b = split(base);
        (!b.is_empty() && p.len() >= b.len() && p[..b.len()] == b[..]).then(|| p[b.len()..].to_vec())
    };
    if let Some(rest) = under(desktop).filter(|r| !r.is_empty()) {
        out.push(names(&rest));
    }
    if let Some(rest) = under(profile) {
        out.push([vec![Part::Clsid(USERS_FILES)], names(&rest)].concat());
    }
    // in a known folder: from it at the top, from This PC's, from Home's
    for (dir, ids) in &at.known {
        if let Some(rest) = under(dir) {
            for id in ids {
                for top in [vec![], vec![Part::Clsid(MY_COMPUTER)], vec![Part::Clsid(HOME)]] {
                    out.push([top, vec![Part::Clsid(*id)], names(&rest)].concat());
                }
            }
        }
    }
    out.push([vec![Part::Clsid(MY_COMPUTER), Part::Drive(drive)], names(&p[1..])].concat());
    out
}

/// A node's children (value name, item), newest first.
fn children(node: &Key) -> Vec<(String, Vec<u8>)> {
    let order: Vec<u32> = node.get("MRUListEx").unwrap_or_default().chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).take_while(|&i| i != u32::MAX).collect();
    order.into_iter().filter_map(|i| node.get(&i.to_string()).map(|v| (i.to_string(), v))).collect()
}

/// The node of `chain` under `root`, as deep as it goes: the nodes found (each with its value name), and how many
/// of the chain's parts they are.
fn walk(root: Key, chain: &[Part], write: bool) -> Vec<(String, Key)> {
    let mut nodes = vec![(String::new(), root)];
    for part in chain {
        let node = &nodes.last().unwrap().1;
        let Some(k) = children(node).into_iter().find(|(_, item)| part_of(item).as_ref() == Some(part)).and_then(|(n, _)| node.sub(&n, write).map(|k| (n, k))) else { break };
        nodes.push(k);
    }
    nodes
}

// -- reading
/// Explorer's view of folder `path`, the newest of the ways it was reached; None if it has none.
pub fn read(base: &str, path: &str, at: &Places) -> Option<View> {
    chains(path, at)
        .iter()
        .filter_map(|c| {
            let mut nodes = walk(Key::open(HKEY_CURRENT_USER, &format!(r"{base}\BagMRU"), false)?, c, false);
            if nodes.len() != c.len() + 1 {
                return None;
            }
            let slot = nodes.pop()?.1.dword("NodeSlot")?;
            bag_view(&Key::open(HKEY_CURRENT_USER, &format!(r"{base}\Bags\{slot}\Shell"), false)?)
        })
        .max_by_key(|v| v.when)
}

/// A bag's view: of its folder types, the one saved last.
fn bag_view(shell: &Key) -> Option<View> {
    shell
        .subkeys()
        .iter()
        .filter_map(|g| shell.sub(g, false))
        .filter_map(|k| {
            let (mode, icon) = (k.dword("Mode")?, k.dword("IconSize").unwrap_or(16));
            Some(View { grid: matches!(mode, 1 | 2 | 5 | 7), icon, sort: k.get("Sort").and_then(|s| sort_of(&s)), when: k.written() })
        })
        .max_by_key(|v| v.when)
}

/// A `Sort` value's first column Mosaic has: 16 bytes of zeros, a count, then each (FMTID, PID, ±1).
pub fn sort_of(b: &[u8]) -> Option<(&'static str, bool)> {
    let n = u32::from_le_bytes(b.get(16..20)?.try_into().ok()?) as usize;
    (0..n.min(16)).find_map(|i| {
        let e = b.get(20 + i * 24..44 + i * 24)?;
        let (fmt, pid, dir) = (&e[..16], u32::from_le_bytes(e[16..20].try_into().ok()?), i32::from_le_bytes(e[20..24].try_into().ok()?));
        let col = match (fmt == STORAGE, pid) {
            (true, 10) => "name",
            (true, 12) => "size",
            (true, 4) => "type",
            (true, 14) => "mtime",
            _ if fmt == ITEM_DATE && pid == 100 => "mtime",
            _ => return None, // a search's rank, a column Mosaic has not: the next one
        };
        Some((col, dir < 0))
    })
}

/// Mosaic's column `col` (and direction) as a `Sort` value.
pub fn sort_value(col: &str, desc: bool) -> Vec<u8> {
    let pid: u32 = match col {
        "size" => 12,
        "type" => 4,
        "mtime" => 14,
        _ => 10, // name; and the folder (Path), all the same in a folder
    };
    let mut v = vec![0u8; 16];
    v.extend(1u32.to_le_bytes());
    v.extend(STORAGE);
    v.extend(pid.to_le_bytes());
    v.extend((if desc { -1i32 } else { 1 }).to_le_bytes());
    v
}

// -- writing
/// What a bag is told.
#[derive(Clone, PartialEq, Debug)]
pub struct Set {
    pub grid: bool,
    pub icon: u32,
    pub sort: String,
    pub desc: bool,
    pub kind: String, // the folder type to file it under when it has none yet ("Pictures", or "Generic")
}

/// Writes `v` into every bag Explorer keeps for `path`. False if it keeps none: [`create`] then adds it.
pub fn update(base: &str, path: &str, at: &Places, v: &Set) -> bool {
    let mut any = false;
    for c in chains(path, at) {
        let Some(root) = Key::open(HKEY_CURRENT_USER, &format!(r"{base}\BagMRU"), true) else { return false };
        let mut nodes = walk(root, &c, true);
        if nodes.len() != c.len() + 1 {
            continue;
        }
        let node = nodes.pop().unwrap().1;
        let slot = match node.dword("NodeSlot") {
            Some(s) => s,
            None => match new_slot(base) {
                Some(s) if node.set_dword("NodeSlot", s) => s,
                _ => continue,
            },
        };
        any |= put_bag(base, slot, v);
    }
    any
}

/// A slot of its own for a new bag: the next byte of the root's `NodeSlots`.
fn new_slot(base: &str) -> Option<u32> {
    let root = Key::open(HKEY_CURRENT_USER, &format!(r"{base}\BagMRU"), true)?;
    let mut slots = root.get("NodeSlots").unwrap_or_default();
    if slots.len() >= MAX_SLOTS {
        return None;
    }
    slots.push(2);
    root.set("NodeSlots", REG_BINARY, &slots).then_some(slots.len() as u32)
}

/// `v` into bag `slot`: each folder type it holds, or the one `v` says when it holds none.
fn put_bag(base: &str, slot: u32, v: &Set) -> bool {
    let Some(shell) = Key::open(HKEY_CURRENT_USER, base, true).and_then(|b| b.create(&format!(r"Bags\{slot}\Shell"))) else { return false };
    let mut types = shell.subkeys().into_iter().filter(|g| g.starts_with('{')).collect::<Vec<_>>();
    let fresh = types.is_empty();
    if fresh {
        let kind = shell.text("SniffedFolderType").unwrap_or_else(|| v.kind.clone());
        let g = TYPES.iter().find(|t| t.0 == kind).unwrap_or(&TYPES[0]);
        if shell.text("SniffedFolderType").is_none() {
            shell.set_text("SniffedFolderType", g.0);
        }
        types.push(g.1.to_string());
    }
    let mut ok = true;
    for g in types {
        let Some(k) = shell.create(&g) else {
            ok = false;
            continue;
        };
        if fresh {
            // as Explorer writes a new one
            k.set_dword("Rev", 0);
            k.set_dword("FFlags", 0x4120_0011);
            k.set_dword("GroupView", 0);
            k.set_text("GroupByKey:FMTID", "{00000000-0000-0000-0000-000000000000}");
            k.set_dword("GroupByKey:PID", 0);
            k.set_dword("GroupByDirection", 1);
        }
        let (mode, lvm, icon, vid) = if v.grid { (1, 3, v.icon.clamp(16, 256), VID_ICONS) } else { (4, 1, 16, VID_DETAILS) };
        ok &= k.set_dword("Mode", mode) && k.set_dword("LogicalViewMode", lvm) && k.set_dword("IconSize", icon) && k.set_text("Vid", vid) && k.set("Sort", REG_BINARY, &sort_value(&v.sort, v.desc));
    }
    ok
}

/// Adds folder `path` to Explorer's tree as the shell names it (`items`: its item ID list, each item with its
/// size), the levels it lacks after the ones it has, then writes `v` into its new bag.
pub fn create(base: &str, items: &[Vec<u8>], v: &Set) -> Result<u32, &'static str> {
    let root = Key::open(HKEY_CURRENT_USER, &format!(r"{base}\BagMRU"), true).ok_or("no BagMRU")?;
    let parts: Vec<Part> = items.iter().map(|i| part_of(i).ok_or("an item of a kind not matched")).collect::<Result<_, _>>()?;
    let mut nodes = walk(root, &parts, true);
    for item in &items[nodes.len() - 1..] {
        let parent = &nodes.last().unwrap().1;
        let taken: Vec<u32> = children(parent).iter().filter_map(|c| c.0.parse().ok()).collect();
        // the next number, past any value or subkey already there (an order list can lag behind)
        let n = (0..).find(|n| !taken.contains(n) && parent.get(&n.to_string()).is_none() && parent.sub(&n.to_string(), false).is_none()).unwrap_or(0u32);
        let mut stored = item.clone();
        stored.extend([0, 0]); // as Explorer keeps it: a one-item list
        let mut order = parent.get("MRUListEx").unwrap_or_default();
        if order.len() < 4 || order.len() % 4 != 0 {
            order = u32::MAX.to_le_bytes().to_vec();
        }
        order.splice(0..0, n.to_le_bytes());
        let name = n.to_string();
        if !parent.set(&name, REG_BINARY, &stored) || !parent.set("MRUListEx", REG_BINARY, &order) {
            return Err("can't write its parent");
        }
        let node = parent.create(&name).ok_or("can't add its node")?;
        node.set("MRUListEx", REG_BINARY, &u32::MAX.to_le_bytes());
        nodes.push((name, node));
    }
    let node = &nodes.last().unwrap().1;
    let slot = match node.dword("NodeSlot") {
        Some(s) => s,
        None => {
            let s = new_slot(base).ok_or("no slot left")?;
            node.set_dword("NodeSlot", s);
            s
        }
    };
    put_bag(base, slot, v).then_some(slot).ok_or("can't write its bag")
}

/// `path`'s item ID list as the shell parses it, each item with its size (its last zero left off).
pub fn items_of(path: &str) -> Option<Vec<Vec<u8>>> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{ILFree, SHParseDisplayName};
    unsafe {
        let mut pidl = null_mut();
        SHParseDisplayName(PCWSTR(wz(path).as_ptr()), None, &mut pidl, 0, None).ok()?;
        let mut out = Vec::new();
        let mut p = pidl as *const u8;
        loop {
            let cb = u16::from_le_bytes([*p, *p.add(1)]) as usize;
            if cb == 0 {
                break;
            }
            out.push(std::slice::from_raw_parts(p, cb).to_vec());
            p = p.add(cb);
        }
        ILFree(Some(pidl));
        Some(out)
    }
}

/// Where the user's own folders are: what [`chains`] goes by.
#[derive(Default)]
pub struct Places {
    pub profile: String,
    pub desktop: String,
    pub known: Vec<(String, [[u8; 16]; 2])>, // each of `KNOWN`'s folders, where it is
}

/// The user's own folder, Desktop and known folders, as the shell has them.
pub fn places() -> Places {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::*;
    let get = |id| unsafe {
        SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, HANDLE::default()).ok().map(|p| {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as _));
            s
        })
    };
    let ids = [&FOLDERID_Desktop, &FOLDERID_Documents, &FOLDERID_Downloads, &FOLDERID_Music, &FOLDERID_Pictures, &FOLDERID_Videos];
    Places {
        profile: get(&FOLDERID_Profile).unwrap_or_default(),
        desktop: get(&FOLDERID_Desktop).unwrap_or_default(),
        known: ids.into_iter().zip(KNOWN).filter_map(|(id, k)| Some((get(id as *const _)?, k))).collect(),
    }
}

/// Explorer's icon size as Mosaic's tile, and back: both are the picture's width in DPI-scaled pixels, so one is
/// the other, within what each can show (Mosaic's smallest tile is 32; Explorer's small icons, 16, are that).
pub fn tile_of(icon: u32) -> i32 {
    icon.clamp(32, 256) as i32
}
pub fn icon_of(tile: i32) -> u32 {
    tile.clamp(16, 256) as u32
}

/// `--bag-write <list|grid> <icon> <col> <asc|desc> <kind> <path>`: Explorer's view of a folder it has no bag
/// for, its tree's missing levels added (run apart: the shell parses the path). `--bag-read <path>` prints it.
pub fn cli(args: &[String]) -> i32 {
    let at = places();
    match args {
        [cmd, path] if cmd == "--bag-read" => {
            match read(BASE, path, &at) {
                Some(v) => println!("{} {} {} {}", if v.grid { "grid" } else { "list" }, v.icon, v.sort.map_or("-", |s| s.0), v.sort.map_or("-", |s| if s.1 { "desc" } else { "asc" })),
                None => println!("none"),
            }
            0
        }
        [cmd, view, icon, col, dir, kind, path] if cmd == "--bag-write" => {
            let v = Set { grid: view == "grid", icon: icon.parse().unwrap_or(96), sort: col.clone(), desc: dir == "desc", kind: kind.clone() };
            if update(BASE, path, &at, &v) {
                println!("updated");
                return 0;
            }
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
            }
            match items_of(path).ok_or("the shell can't parse it").and_then(|items| create(BASE, &items, &v)) {
                Ok(slot) => {
                    println!("created {slot}");
                    0
                }
                Err(e) => {
                    println!("failed: {e}");
                    1
                }
            }
        }
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file system folder's item, as Explorer makes one (version 9 extension block, its long name inside).
    fn fs_item(name: &str) -> Vec<u8> {
        let mut v = vec![0, 0, 0x31, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0];
        v.extend(b"SHORT~1\0");
        let wname: Vec<u8> = name.encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect();
        let mut ext = vec![0u8; 0x2E];
        ext[2..4].copy_from_slice(&9u16.to_le_bytes());
        ext[4..8].copy_from_slice(&0xBEEF0004u32.to_le_bytes());
        ext.extend(&wname);
        ext.extend(((v.len()) as u16).to_le_bytes());
        let n = ext.len() as u16;
        ext[0..2].copy_from_slice(&n.to_le_bytes());
        v.extend(ext);
        let n = v.len() as u16;
        v[0..2].copy_from_slice(&n.to_le_bytes());
        v
    }
    fn clsid_item(g: [u8; 16]) -> Vec<u8> {
        let mut v = vec![20, 0, 0x1F, 0x50];
        v.extend(g);
        v
    }
    fn drive_item(l: u8) -> Vec<u8> {
        let mut v = vec![25, 0, 0x2F, l, b':', b'\\'];
        v.resize(25, 0);
        v
    }

    #[test]
    fn items_name_what_paths_go_through() {
        assert_eq!(part_of(&fs_item("Été Photos")), Some(Part::Name("été photos".into())));
        assert_eq!(part_of(&clsid_item(MY_COMPUTER)), Some(Part::Clsid(MY_COMPUTER)));
        assert_eq!(part_of(&drive_item(b'd')), Some(Part::Drive(b'D')));
        assert_eq!(part_of(&[3, 0, 0x31]), None);
        // Home's Pictures: an item list This PC > Pictures inside a property store
        let mut home = vec![0, 0, 0x79, 0];
        home.extend([0u8; 30]);
        home.extend(clsid_item(MY_COMPUTER));
        let mut pics = clsid_item(KNOWN[4][0]);
        pics[2] = 0x2E;
        home.extend(pics);
        home.extend([0u8; 30]);
        assert_eq!(part_of(&home), Some(Part::Clsid(KNOWN[4][0])));
    }

    #[test]
    fn a_path_is_reached_the_ways_explorer_reaches_it() {
        let at = me();
        let c = chains(r"C:\Users\Me\Desktop\Work\Old", &at);
        let n = |s: &str| Part::Name(s.into());
        assert_eq!(c[0], vec![n("work"), n("old")]);
        assert_eq!(c[1], vec![Part::Clsid(USERS_FILES), n("desktop"), n("work"), n("old")]);
        assert_eq!(c[2], vec![Part::Clsid(MY_COMPUTER), Part::Drive(b'C'), n("users"), n("me"), n("desktop"), n("work"), n("old")]);
        assert_eq!(chains(r"D:\", &at), vec![vec![Part::Clsid(MY_COMPUTER), Part::Drive(b'D')]]);
        assert!(chains(r"\\server\share", &at).is_empty());
        // in Pictures: also from Pictures at the top, This PC's and Home's, by either of its ids
        let c = chains(r"C:\Users\Me\Pictures\Shots", &at);
        assert!(c.contains(&vec![Part::Clsid(HOME), Part::Clsid(KNOWN[4][0]), n("shots")]));
        assert!(c.contains(&vec![Part::Clsid(KNOWN[4][1]), n("shots")]));
        assert!(c.contains(&vec![Part::Clsid(MY_COMPUTER), Part::Drive(b'C'), n("users"), n("me"), n("pictures"), n("shots")]));
    }
    fn me() -> Places {
        Places { profile: r"C:\Users\Me".into(), desktop: r"C:\Users\Me\Desktop".into(), known: vec![(r"C:\Users\Me\Pictures".into(), KNOWN[4])] }
    }

    #[test]
    fn sizes_and_sorts_go_both_ways() {
        for size in [48, 96, 160, 256] {
            assert_eq!(icon_of(tile_of(size)), size);
        }
        assert_eq!((tile_of(16), tile_of(1000)), (32, 256));
        for (c, d) in [("name", false), ("size", true), ("type", false), ("mtime", true)] {
            assert_eq!(sort_of(&sort_value(c, d)), Some((c, d)));
        }
        // Explorer's own: search rank first (skipped), then date modified descending
        let s = hex("0000000000000000000000000000000003000000901c6949177e1a10a91c08002b2ecda903000000ffffffff30f125b7ef471a10a5f102608c9eebac0e000000ffffffff30f125b7ef471a10a5f102608c9eebac0a00000001000000");
        assert_eq!(sort_of(&s), Some(("mtime", true)));
    }
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// The whole round on a scratch copy of the tree's shape (never Explorer's own key).
    #[test]
    fn a_folder_added_updated_and_read_back() {
        let base = r"Software\Mosaic\BagTest\Shell";
        let top = Key::open(HKEY_CURRENT_USER, "Software\\Mosaic", true).or_else(|| Key::open(HKEY_CURRENT_USER, "Software", true)?.create("Mosaic")).unwrap();
        unsafe { RegDeleteTreeW(top.0, wz("BagTest").as_ptr()) };
        let mru = top.create(r"BagTest\Shell\BagMRU").unwrap();
        mru.set("NodeSlots", REG_BINARY, &[2, 2]);
        mru.set_dword("NodeSlot", 1);
        mru.set("MRUListEx", REG_BINARY, &u32::MAX.to_le_bytes());
        let at = me();
        let set = |grid, icon, sort: &str, desc| Set { grid, icon, sort: sort.into(), desc, kind: "Pictures".into() };

        // nothing yet: no view, and an update finds no bag
        assert_eq!(read(base, r"C:\Data\Photos", &at), None);
        assert!(!update(base, r"C:\Data\Photos", &at, &set(true, 96, "mtime", true)));
        // added from its item list: every level, a new slot past the two taken
        let items = vec![clsid_item(MY_COMPUTER), drive_item(b'C'), fs_item("Data"), fs_item("Photos")];
        assert_eq!(create(base, &items, &set(true, 96, "mtime", true)), Ok(3));
        let v = read(base, r"c:\data\PHOTOS\", &at).unwrap();
        assert_eq!((v.grid, v.icon, v.sort), (true, 96, Some(("mtime", true))));
        let shell = Key::open(HKEY_CURRENT_USER, &format!(r"{base}\Bags\3\Shell"), false).unwrap();
        assert_eq!(shell.text("SniffedFolderType").as_deref(), Some("Pictures"));
        assert_eq!(shell.subkeys(), vec![TYPES[1].1.to_string()]);
        // a sibling: the levels it shares are reused, one node and one slot added
        let items2 = vec![clsid_item(MY_COMPUTER), drive_item(b'C'), fs_item("Data"), fs_item("Music")];
        assert_eq!(create(base, &items2, &set(false, 16, "name", false)), Ok(4));
        let data = Key::open(HKEY_CURRENT_USER, &format!(r"{base}\BagMRU\0\0\0"), false).unwrap();
        assert_eq!(children(&data).iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), vec!["1", "0"]); // newest first
        assert_eq!(Key::open(HKEY_CURRENT_USER, &format!(r"{base}\BagMRU"), false).unwrap().get("NodeSlots").unwrap(), vec![2, 2, 2, 2]);
        // updated in place: list, by size; its other values left as they were
        assert!(update(base, r"C:\Data\Photos", &at, &set(false, 16, "size", false)));
        let v = read(base, r"C:\Data\Photos", &at).unwrap();
        assert_eq!((v.grid, v.sort), (false, Some(("size", false))));
        let k = Key::open(HKEY_CURRENT_USER, &format!(r"{base}\Bags\3\Shell\{}", TYPES[1].1), false).unwrap();
        assert_eq!((k.dword("Mode"), k.dword("FFlags"), k.text("Vid").as_deref()), (Some(4), Some(0x4120_0011), Some(VID_DETAILS)));
        // a level node with no bag of its own (Data) gets a slot when it's written
        assert!(update(base, r"C:\Data", &at, &set(true, 48, "name", false)));
        assert_eq!(read(base, r"C:\Data", &at).map(|v| v.icon), Some(48));
        assert_eq!(data.dword("NodeSlot"), Some(5));
        unsafe { RegDeleteTreeW(top.0, wz("BagTest").as_ptr()) };
    }
}
