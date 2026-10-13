//! Aggregates one [`SearchIndex`] per volume into a single searchable catalog.
//! Each volume keeps its own MFT record numbering and path root; searches fan
//! out across volumes and merge into one globally-ranked result set.

use std::time::Instant;

use parking_lot::Mutex;
use rayon::prelude::*;

use crate::application::index::{Hit, SearchIndex, SearchResult};
use crate::application::search::{extract_distinct, Matcher, SearchOptions, SortKey};

/// All indexed volumes.
#[derive(Default)]
pub struct Catalog {
    volumes: Vec<SearchIndex>,
    /// Bumped on every mutation, so a cached view never outlives the entries it points at.
    generation: u64,
    /// The last query's full ranked order: paging (scrolling) slices it instead of re-searching.
    view: Mutex<Option<View>>,
    /// The last few folder listings' orders (each pane's, Home): a live refresh asks again every couple of seconds
    /// while the disk is busy elsewhere, and gets these back unless something in the folder changed.
    listings: Mutex<std::collections::VecDeque<Listing>>,
    /// Indexes swapped in (`upsert_volume`).
    swaps: u64,
}

struct View {
    key: String,
    generation: u64,
    /// The index's rebuild epoch: positions from an older one point at other entries.
    epoch: u64,
    /// The index's change stamp the order is current with (see `SearchIndex::patch_all`).
    stamp: u64,
    order: Vec<u32>,
}

struct Listing {
    key: String,
    epoch: u64,
    /// The folders listed, and the index's change stamp when they were (see `SearchIndex::changed_under`).
    dirs: Vec<u32>,
    stamp: u64,
    order: Vec<u32>,
}
/// Listings kept, and the most entries one may hold (a drive root's `descendant`-sized listing isn't worth it).
const LISTINGS: usize = 8;
const LISTING_MAX: usize = 100_000;

impl Catalog {
    /// Insert a freshly-built volume index, replacing any prior index for the
    /// same drive.
    pub fn upsert_volume(&mut self, index: SearchIndex) {
        let drive = index.drive();
        self.generation += 1;
        self.listings.get_mut().clear(); // a new index counts its changes and epochs from zero again
        *self.view.get_mut() = None;
        self.swaps += 1;
        match self.volumes.iter_mut().find(|v| v.drive() == drive) {
            Some(slot) => *slot = index,
            None => self.volumes.push(index),
        }
    }

    /// Mutable access to a volume's index (used by its USN watcher).
    pub fn volume_mut(&mut self, drive: char) -> Option<&mut SearchIndex> {
        self.generation += 1;
        self.volumes.iter_mut().find(|v| v.drive() == drive)
    }

    /// Shared access to a volume's index (used by the cache saver).
    pub fn volume(&self, drive: char) -> Option<&SearchIndex> {
        self.volumes.iter().find(|v| v.drive() == drive)
    }

    /// Where every live, non-directory file is (volume, position) and each volume's rebuild epoch. The content scan
    /// reads the files from these in chunks (`files_at`): `all_files()` holds a ~120-byte path per file all at once,
    /// hundreds of MB over millions of files, for as long as the scan runs.
    pub fn file_positions(&self) -> (Vec<(u16, u32)>, Vec<u64>) {
        let mut refs = Vec::new();
        for (n, v) in self.volumes.iter().enumerate() {
            refs.extend(v.live_name_order().into_iter().filter(|&i| !v.hit(i).is_dir).map(|i| (n as u16, i)));
        }
        (refs, self.volumes.iter().map(|v| v.epoch()).collect())
    }

    /// The (path, mtime unix-seconds, size) of `refs`; `None` once an index was rebuilt since `file_positions`
    /// (its positions then point at other entries).
    pub fn files_at(&self, refs: &[(u16, u32)], epochs: &[u64]) -> Option<Vec<(String, i64, i64)>> {
        if self.volumes.iter().map(|v| v.epoch()).ne(epochs.iter().copied()) {
            return None;
        }
        Some(refs.iter().filter(|&&(n, i)| self.volumes[n as usize].contains(i)).map(|&(n, i)| {
            let h = self.volumes[n as usize].hit(i);
            (h.path, h.modified / 1000, h.size)
        }).collect())
    }

    /// Search every volume and return one merged, ranked, truncated result set.
    pub fn search(&self, opts: &SearchOptions) -> SearchResult {
        let start = Instant::now();
        // `distinct:`/`unique:` aggregates over the whole ranked result, not a per-entry test, so
        // it's pulled out of the query before compiling and applied after ranking below.
        let (query, distinct) = extract_distinct(&opts.query);
        let opts_owned;
        let opts = if distinct.is_some() {
            opts_owned = SearchOptions { query, ..opts.clone() };
            &opts_owned
        } else {
            opts
        };
        let matcher = match Matcher::compile(opts) {
            Ok(m) => m,
            Err(e) => return SearchResult::error(e, start.elapsed().as_millis()),
        };

        let mut result = match self.volumes.as_slice() {
            [] => SearchResult {
                total: 0,
                took_ms: 0,
                hits: Vec::new(),
                error: None,
                capped: false,
                stamp: None,
            },
            [only] => {
                // a listing's version: its stamp, told apart from a rescanned index's (which counts from zero again)
                let version = |stamp: u64| stamp ^ self.swaps << 48;
                let page = |order: &[u32], stamp: Option<u64>| SearchResult {
                    total: order.len(),
                    took_ms: 0,
                    hits: order.iter().skip(opts.offset).take(opts.limit).filter(|&&i| only.contains(i)).map(|&i| only.hit(i)).collect(),
                    error: None,
                    capped: false,
                    stamp: stamp.map(version),
                };
                // everything but the page window identifies the ranked order; a distinct field
                // changes what that order contains, so it must be part of the cache key too.
                let mut key = serde_json::to_string(&SearchOptions { offset: 0, limit: 0, ..opts.clone() }).unwrap_or_default();
                if let Some(f) = &distinct {
                    key += &format!("\0distinct={f}");
                }
                // Only a new or refreshed search (its first page) re-ranks after index changes; later pages reuse its
                // view, so disk churn (a re-rank every second or so) never stalls a scroll.
                // ponytail: rows can show a renamed/deleted entry until the next refresh (the UI re-runs on index change)
                let epoch = only.epoch();
                if let Some(v) = self.view.lock().as_mut().filter(|v| v.key == key && v.epoch == epoch) {
                    if opts.offset > 0 || v.generation == self.generation {
                        return page(&v.order, None);
                    }
                    // everything, by name (the empty search box): take in just what changed since
                    if matcher.matches_all() && matches!(opts.sort, SortKey::Name) && distinct.is_none() && only.patch_all(&mut v.order, v.stamp, opts) {
                        (v.generation, v.stamp) = (self.generation, only.stamp());
                        return page(&v.order, None);
                    }
                }
                // a folder listing: the same order again unless something in or below the folder changed
                let dirs = if distinct.is_none() { only.listing_dirs(&matcher) } else { None };
                if let Some(dirs) = &dirs {
                    let mut ls = self.listings.lock();
                    if let Some(k) = ls.iter().position(|l| l.key == key && l.epoch == epoch && l.dirs == *dirs) {
                        if !only.changed_under(dirs, ls[k].stamp) {
                            let l = ls.remove(k).unwrap();
                            let res = page(&l.order, Some(l.stamp));
                            ls.push_back(l); // most recently used last
                            return res;
                        }
                        ls.remove(k);
                    }
                }
                let stamp = only.stamp();
                let mut order = only.ranked(opts, &matcher); // outside the lock: pages of the current view keep flowing
                if let Some(field) = &distinct {
                    order = dedup_order(only, order, field);
                }
                let res = page(&order, dirs.as_ref().map(|_| stamp));
                if let Some(dirs) = dirs.filter(|_| order.len() <= LISTING_MAX) {
                    let mut ls = self.listings.lock();
                    if ls.len() == LISTINGS {
                        ls.pop_front();
                    }
                    ls.push_back(Listing { key: key.clone(), epoch, dirs, stamp, order: order.clone() });
                }
                // kept only when there are pages left to fetch: a result that fits in one page (the folder-icon
                // checks, `folder_kind`) would otherwise replace the list's view and make its next page re-rank
                if order.len() > opts.offset + opts.limit {
                    *self.view.lock() = Some(View { order, key, generation: self.generation, epoch, stamp });
                }
                res
            }
            // ponytail: no view cache across volumes (each page re-ranks); add a merged view if multi-drive scrolling lags
            many => {
                // each volume returns its top offset+limit; the merged list is then paged
                let mut per = opts.clone();
                per.limit = opts.offset + opts.limit;
                per.offset = 0;
                let parts: Vec<SearchResult> =
                    many.par_iter().map(|v| v.search(&per, &matcher)).collect();
                let mut total = parts.iter().map(|p| p.total).sum();
                let mut hits: Vec<Hit> = parts.into_iter().flat_map(|p| p.hits).collect();
                sort_hits(&mut hits, opts);
                if let Some(field) = &distinct {
                    dedup_hits(&mut hits, field);
                    total = hits.len(); // the per-volume sum above double-counts what dedup then drops
                }
                hits.drain(..opts.offset.min(hits.len()));
                hits.truncate(opts.limit);
                SearchResult {
                    total,
                    took_ms: 0,
                    hits,
                    error: None,
                    capped: false,
                    stamp: None,
                }
            }
        };

        result.took_ms = start.elapsed().as_millis();
        result
    }
}

/// Re-rank merged hits from multiple volumes by the requested column.
fn sort_hits(hits: &mut [Hit], opts: &SearchOptions) {
    if opts.folders_first {
        let asc = opts.ascending;
        let sort = opts.sort;
        // Directories first, then ordered within each group by the column.
        hits.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| ordered(hit_col_cmp(a, b, sort), asc))
        });
        return;
    }
    match opts.sort {
        SortKey::Name => hits.sort_by_key(|h| h.name.to_lowercase()),
        SortKey::Path => hits.sort_by_key(|h| h.path.to_lowercase()),
        SortKey::Size => hits.sort_by_key(|h| h.size),
        SortKey::Modified => hits.sort_by_key(|h| h.modified),
        SortKey::Created => hits.sort_by_key(|h| h.created),
        SortKey::Accessed => hits.sort_by_key(|h| h.accessed),
        SortKey::Ext => hits.sort_by_key(hit_ext),
        SortKey::Attributes => hits.sort_by_key(|h| h.attributes),
    }
    if !opts.ascending {
        hits.reverse();
    }
}

fn ordered(ord: std::cmp::Ordering, ascending: bool) -> std::cmp::Ordering {
    if ascending {
        ord
    } else {
        ord.reverse()
    }
}

/// Lowercased extension of a hit's name (after the last dot), or `""`. A leading
/// dot is a dotfile, not an extension, and directories have no extension —
/// matching the UI's Ext column so the sort agrees with what's displayed.
fn hit_ext(h: &Hit) -> String {
    if h.is_dir {
        return String::new();
    }
    match h.name.rfind('.') {
        Some(i) if i > 0 => h.name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// `distinct:field` key for one hit: `field` is `name` (case-insensitive), `ext`, or `size`.
fn dedup_key(h: &Hit, field: &str) -> String {
    match field {
        "ext" => hit_ext(h),
        "size" => h.size.to_string(),
        _ => h.name.to_lowercase(),
    }
}

/// Drop every hit after the first with a given `distinct:field` value, keeping ranked order.
fn dedup_hits(hits: &mut Vec<Hit>, field: &str) {
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| seen.insert(dedup_key(h, field)));
}

/// Same as `dedup_hits`, but over a single volume's ranked record order directly, so the caller
/// can cache the already-deduped order (see the view cache above) instead of re-deduping per page.
fn dedup_order(idx: &SearchIndex, order: Vec<u32>, field: &str) -> Vec<u32> {
    let mut seen = std::collections::HashSet::new();
    order.into_iter().filter(|&i| seen.insert(dedup_key(&idx.hit(i), field))).collect()
}

/// Compare two merged hits by a sort column, ascending.
fn hit_col_cmp(a: &Hit, b: &Hit, sort: SortKey) -> std::cmp::Ordering {
    match sort {
        SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        SortKey::Path => a.path.to_lowercase().cmp(&b.path.to_lowercase()),
        SortKey::Size => a.size.cmp(&b.size),
        SortKey::Modified => a.modified.cmp(&b.modified),
        SortKey::Created => a.created.cmp(&b.created),
        SortKey::Accessed => a.accessed.cmp(&b.accessed),
        SortKey::Ext => hit_ext(a).cmp(&hit_ext(b)),
        SortKey::Attributes => a.attributes.cmp(&b.attributes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(name: &str, is_dir: bool) -> Hit {
        Hit {
            name: name.into(),
            path: format!("C:\\{name}"),
            size: if is_dir { -1 } else { 1 },
            modified: 0,
            created: 0,
            accessed: 0,
            attributes: 0,
            is_dir,
        }
    }

    fn opts(sort: SortKey, ascending: bool, folders_first: bool) -> SearchOptions {
        SearchOptions {
            sort,
            ascending,
            folders_first,
            ..SearchOptions::default()
        }
    }

    fn names(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn folders_first_groups_dirs_then_sorts_within() {
        let mut hits = vec![
            hit("b.txt", false),
            hit("zdir", true),
            hit("a.txt", false),
            hit("adir", true),
        ];
        sort_hits(&mut hits, &opts(SortKey::Name, true, true));
        assert_eq!(names(&hits), ["adir", "zdir", "a.txt", "b.txt"]);
    }

    #[test]
    fn folders_first_keeps_dirs_on_top_when_descending() {
        let mut hits = vec![hit("b.txt", false), hit("adir", true), hit("a.txt", false)];
        sort_hits(&mut hits, &opts(SortKey::Name, false, true));
        // Dirs stay grouped on top; files within the group reverse.
        assert_eq!(names(&hits), ["adir", "b.txt", "a.txt"]);
    }

    #[test]
    fn ext_sort_orders_by_extension() {
        let mut hits = vec![
            hit("a.zip", false),
            hit("b.doc", false),
            hit("c.png", false),
        ];
        sort_hits(&mut hits, &opts(SortKey::Ext, true, false));
        assert_eq!(names(&hits), ["b.doc", "c.png", "a.zip"]);
    }

    #[test]
    fn extract_distinct_pulls_the_field_and_cleans_the_query() {
        let (rest, field) = extract_distinct("report distinct:ext memo");
        assert_eq!(rest, "report memo");
        assert_eq!(field.as_deref(), Some("ext"));

        let (rest, field) = extract_distinct("just a name search");
        assert_eq!(rest, "just a name search");
        assert!(field.is_none());

        // An unrecognised field drops the operator without touching the query's `name` field kind.
        let (rest, field) = extract_distinct("report distinct:bogus");
        assert_eq!(rest, "report");
        assert!(field.is_none());
    }

    #[test]
    fn dedup_hits_keeps_first_occurrence_by_field() {
        let mut hits = vec![hit("a.zip", false), hit("b.zip", false), hit("c.doc", false)];
        dedup_hits(&mut hits, "ext");
        assert_eq!(names(&hits), ["a.zip", "c.doc"]);

        let mut same_size = vec![
            Hit { size: 100, ..hit("a.txt", false) },
            Hit { size: 200, ..hit("b.txt", false) },
            Hit { size: 100, ..hit("c.txt", false) },
        ];
        dedup_hits(&mut same_size, "size");
        assert_eq!(names(&same_size), ["a.txt", "b.txt"]);
    }
}
