# Everything (voidtools) vs. this app

Exhaustive gap analysis. voidtools' own docs (search syntax, search modifiers, search
functions pages) were fetched directly for this pass — Everything has ~250 named
search functions and dozens of UI/display functions, so metadata-property functions
are grouped by theme below rather than listed one-by-one; everything else structural
(regex, ETP, SDK, dupe finder, etc.) is listed individually. The codebase side was
re-checked file by file (`main.rs`, `native.rs`, `native_preview.rs`, `metadata.rs`,
`backend.rs`, `infrastructure/*`).

## Already have
| Feature | Note |
|---|---|
| Instant NTFS filename search | own SYSTEM service, USN journal (`infrastructure/ntfs`) |
| Real-time updates | monitor mode (content) + USN journal (names) |
| `ext:` `size:` `dm:`/`dc:`/`da:`/`date:` `path:` `file:` `folder:` `attrib:` `content:` | `main.rs` `OPS` |
| Wildcards `*` `?`, quoted phrases, `\|` OR, `!` NOT, `()` grouping | passed through to Recoll query syntax |
| Full-text content search, indexed (not scanned live like Everything's `content:`) | Recoll/Xapian — actually stronger than Everything here |
| Sort by Name/Path/Size/Type/Modified, asc/desc | `native.rs` `COLS` |
| Thumbnails, image/video/audio/GIF/text preview pane | `native_preview.rs` |
| Category quick-filters (images, documents, executables, ...) | `main.rs` `CATS` |

## Missing

### Search syntax & modifiers
| Everything has | This app | Notes |
|---|---|---|
| `regex:`/`noregex:` (full PCRE) | ❌ | only Recoll's own query grammar on names |
| `case:`/`nocase:` toggle | ❌ | no case-sensitivity control |
| `diacritics:`/`nodiacritics:` | ❌ | |
| `ascii:`/`noascii:`/`utf8:` | ❌ | |
| `whole-word` matching | ❌ | |
| Double-asterisk `**` (match across path separators) vs `*` | ❌ | only single wildcard form |
| Character entities (`&sp:`, `&vert:`, `&excl:`, `&#<n>:`, ...) for literal special chars in queries | ❌ | |
| Macros: `audio:` `doc:` `exe:` `image:` `video:` `zip:` as one-word shortcuts | ⚠️ partial | we have category buttons (UI), not typed macros in the query itself |

### Search functions — file system & properties
| Everything has | This app |
|---|---|
| `parent:` `child:` `sibling:` `ancestor:` `descendant:` (+ `-file`/`-folder`/`-count` variants) — structural relationship queries | ❌ |
| `dupe:` `dupe-min:` `dupe-max:` `dupe-count:` `dupe-from:` (duplicate-file finder) | ❌ |
| `empty:` (empty folders) | ❌ |
| `distinct:`/`unique:`/`distinct-sort:` (de-duplicated results) | ❌ |
| `count:` (limit result count in-query) | ❌ |
| `exists:` `file-exists:` `folder-exists:` | ❌ |
| `attributes:` beyond our single `attrib:` (hidden/system/readonly/etc. as named filters) | ⚠️ partial |
| `owner:` | ❌ |
| `comment:` `tag:` `rating:` (Explorer-style file metadata) | ❌ |
| `run-count:` `date-run:` (Everything's own usage tracking) | ❌ (no concept of "opened via this app" tracking) |
| `hard-link-count:` `hard-link-filenames:` `alternate-data-stream-*` | ❌ |
| `shortcut-target:` `url:` (.lnk / .url resolution) | ❌ |
| `volume-label:` `network-index-host:` `computer:` | ❌ |
| `offline:` `online:` `offline-availability:` (cloud-placeholder / OneDrive-style files) | ❌ |

### Search functions — hashes & signatures
| Everything has | This app |
|---|---|
| `md5:` `sha1:` `sha256:` `crc32:` `crc64:` | ❌ |
| `digital-signature-name:` `digital-signature-timestamp:` (Authenticode) | ❌ |
| `file-signature:` (magic-byte type sniffing) | ❌ |

### Search functions — dates
| Everything has | This app |
|---|---|
| `date-modified/-created/-accessed/-changed` + component breakdowns (`-year` `-month` `-day` `-day-of-week` `-hour`, etc.) | ⚠️ partial — we have `dm:`/`dc:`/`da:`/`date:` as whole-date filters, no component granularity |
| `date-deleted` (Recycle Bin), `date-recently-changed` (index change) | ❌ |

### Search functions — rich media/document metadata (~60 functions)
| Everything has | This app |
|---|---|
| Photo/EXIF: `aperture` `iso-speed` `focal-length` `exposure-*` `camera-maker/model` `lens-model` `latitude`/`longitude` `orientation` `white-balance` `people`, etc. | ❌ |
| Audio/video: `artist` `album` `genre` `composer` `bit-rate` `sample-rate` `frame-rate` `dimensions` `codec`/`-format` `track` `year` `length`, etc. | ❌ |
| Documents: `authors` `title` `subject` `page-count` `word-count` `line-count` `last-author` `category` `company`, etc. | ❌ |
| Executables: `product-name` `product-version` `copyright` `trademarks` `language` | ❌ |
| Regex capture groups as fields: `regular-expression-match0..9` | ❌ |

We show basic metadata (dimensions, duration, etc.) in the preview pane (`metadata.rs`) but none of it is *searchable/filterable* the way Everything's functions are.

### Sort / grouping / display functions
| Everything has | This app |
|---|---|
| Sort by dozens of properties (size-on-disk, attributes, owner, run-count, any metadata field, ...) | ⚠️ partial — only Name/Path/Size/Type/Modified |
| `sort-mix`/`no-sort-mix` (interleave vs. separate files & folders) | ❌ (folders-first is fixed behavior) |
| `add-columns`/`remove-columns`/`columns:` — arbitrary customizable result columns | ❌ (fixed columns) |
| `bookmarks-sidebar` / `filters-sidebar` / `folders-sidebar` panels | ❌ |
| `on-top`/`no-on-top` (always-on-top window) | ❌ |
| Multiple saved layouts (`layout:`/`add-layout:`/`remove-layout:`) | ❌ |
| Alternate row coloring toggle | ❌ (fixed style) |

### UX
| Everything has | This app |
|---|---|
| Saved searches / bookmarks (with per-bookmark columns, folders of bookmarks) | ❌ |
| Filters sidebar (custom named filter macros) | ❌ |
| Search history (recent queries) | ❌ |
| Multiple search tabs or windows | ❌ |
| Themes (Default/Dark/Light + custom) | ⚠️ partial — dark UI only, no theme switching |
| 15+ UI languages / translations | ❌ (English only) |

### Indexing scope
| Everything has | This app |
|---|---|
| Network share / UNC / other-machine indexing (via ETP) | ❌ — local NTFS volumes only |
| Multiple independent indexes, "index journal" inspection | ❌ |
| Indexing non-NTFS volumes (FAT32/exFAT/removable) without falling back to a slow scan | ❌ |
| Folder-only "index this folder as if it were a drive" (folder indexes) | ❌ |

### Integration & automation
| Everything has | This app |
|---|---|
| Command-line client (`es.exe`) for scripting | ❌ |
| SDK / IPC (DLL + window messages) for other apps to query the index | ❌ |
| ETP server (network protocol so other devices/apps can query this machine) | ❌ |
| HTTP server / "Everything Server" mode | ❌ |
| Explorer right-click "Search Everything here" context-menu integration | ❌ |
| Plugin support | ❌ |
| Run-as-elevated search (see into folders the current user can't list) | ❌ |
| `.efu` file-list import/export, export results to CSV/TXT | ❌ |
| Multiple instance support / `-instance` switch | ❌ (single instance implied, not verified either way) |

## Deliberately not chasing
- Everything's raw NTFS-journal filename engine is already matched by our own USN-journal service — no need to out-Everything Everything there.
- The ~150 rich-metadata search functions (EXIF, audio tags, document properties, hashes) are a genuinely large feature surface (parsers per file format). Worth doing incrementally if/when a specific one is wanted, not as a blanket "implement all of Everything's metadata functions" effort.
- ETP/network search, SDK/IPC, and an HTTP server are real scope increases (a network-facing server, an API surface, auth model) — a deliberate yes/no from the user before building, not a default addition.
