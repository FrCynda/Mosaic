# Third-party notices

This product includes third-party software. This file is a mechanically-generated snapshot
(from `cargo metadata`) of what's in the dependency tree today, grouped by license. It is a
starting point for legal review before shipping, not itself a legal opinion.

As of this snapshot, **every dependency in the build — direct and transitive — is under a
permissive license** (MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0, Unlicense, CC0-1.0, or
Unicode-3.0). None require this application's own source to be disclosed or redistributed
under the same terms. Nothing in the shipped binary is GPL/LGPL-licensed.

## Adapted code

The NTFS filename-search engine (`src/application/`, `src/domain/`, `src/infrastructure/`) is
adapted from **AllTheThings** by Swatto (<https://github.com/xin-521/AllTheThings>), MIT
licensed:

```
MIT License

Copyright (c) 2026 Swatto

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Rust crates, by license

Package lists below are every crate `cargo metadata` reports in the dependency graph (direct
and transitive) as of this snapshot; re-run `cargo metadata --format-version 1` and regenerate
this section after any dependency change. Full license texts for each of these standard,
unmodified license types are their well-known canonical wording (MIT, Apache License 2.0, the
2- and 3-clause BSD licenses, ISC, Zlib, MPL 2.0, The Unlicense, CC0 1.0) — reproduced in full
in each crate's own repository/crates.io page; omitted here for length, per the common
`THIRD-PARTY-NOTICES` convention of listing the license type and package set instead of
repeating boilerplate hundreds of times.

**Content search engine:** `tantivy` and its `tantivy-*` support crates — MIT.

**MIT:** atk, atk-sys, bincode, bitpacking, block2, bytes, cairo-rs, cairo-sys-rs,
cargo_metadata, census, cfb, color_quant, combine, crunchy, darling, darling_core,
darling_macro, derive_more, derive_more-impl, dlopen2, dlopen2_derive, dom_query,
embed-resource, gdk, gdk-pixbuf, gdk-pixbuf-sys, gdk-sys, gdkwayland-sys, gdkx11, gdkx11-sys,
generic-array, gio, gio-sys, glib, glib-macros, glib-sys, gobject-sys, gtk, gtk-sys,
gtk3-macros, http-body, http-body-util, hyper, hyper-util, ico, infer, javascriptcore-rs,
javascriptcore-rs-sys, levenshtein_automata, libm, libredox, lru, lz4_flex, measure_time,
memoffset, mio, murmurhash32, new_debug_unreachable, nom, objc2, objc2-encode,
objc2-foundation, ownedbytes, pango, pango-sys, phf, phf_codegen, phf_generator, phf_macros,
phf_shared, plist, precomputed-hash, quick-xml, redox_syscall, redox_users, schemars,
schemars_derive, simd-adler32, slab, soup3, soup3-sys, strsim, synstructure, tantivy,
tantivy-bitpacker, tantivy-columnar, tantivy-common, tantivy-query-grammar, tantivy-sstable,
tantivy-stacker, tantivy-tokenizer-api, tauri-winres, tokio, tokio-util, tower, tower-http,
tower-layer, tower-service, tracing, tracing-core, try-lock, urlpattern, version-compare,
vswhom, vswhom-sys, winnow, winreg, x11, x11-dl, zip, zmij, zstd.

**MIT OR Apache-2.0** (dual-licensed; used here under MIT or Apache-2.0 at our option):
allocator-api2, android_system_properties, anyhow, arbitrary, arc-swap, async-trait, base64,
bitflags, block-buffer, bumpalo, camino, cargo-platform, cc, cfg-expr, cfg-if, chrono, cookie,
core-foundation, core-foundation-sys, core-graphics, core-graphics-types, cpufeatures,
crc32fast, crossbeam-channel, crossbeam-deque, crossbeam-epoch, crossbeam-utils, crypto-common,
defmt, defmt-macros, defmt-parser, deranged, derive_arbitrary, digest, dirs, dirs-sys,
displaydoc, dtoa, dyn-clone, either, embed_plist, erased-serde, errno, fdeflate, field-offset,
find-msvc-tools, flate2, form_urlencoded, fs4, futures-channel, futures-core,
futures-executor, futures-io, futures-macro, futures-sink, futures-task, futures-util,
getrandom, gif, glob, hashbrown, heck, hermit-abi, hex, html5ever, http, httparse,
iana-time-zone, iana-time-zone-haiku, idna, image, ipnet, itertools, itoa, jni-sys,
jni-sys-macros, jobserver, js-sys, jsonptr, keyboard-types, libc, lock_api, log, markup5ever,
md-5, memmap2, mime, ndk, ndk-sys, num-conv, num-traits, num_cpus, once_cell, oneshot,
parking_lot, parking_lot_core, percent-encoding, pkg-config, png, powerfmt, ppv-lite86,
proc-macro-crate, proc-macro-error, proc-macro-error-attr, proc-macro2, quote, rand,
rand_chacha, rand_core, rand_distr, rayon, rayon-core, ref-cast, ref-cast-impl, regex,
regex-automata, regex-syntax, reqwest, rustc_version, rustversion, scopeguard, semver, serde,
serde-untagged, serde_core, serde_derive, serde_derive_internals, serde_json, serde_repr,
serde_spanned, serde_with, serde_with_macros, serialize-to-javascript,
serialize-to-javascript-impl, servo_arc, sha1, sha2, shlex, siphasher, smallvec, socket2,
softbuffer, stable_deref_trait, string_cache, string_cache_codegen, swift-rs, syn, system-deps,
tao-macros, tempfile, tendril, thiserror, thiserror-impl, time, time-core, time-macros, toml,
toml_datetime, toml_edit, toml_parser, toml_writer, tray-icon, typeid, typenum,
unicode-normalization, unicode-segmentation, url, wasm-bindgen, wasm-bindgen-futures,
wasm-bindgen-macro, wasm-bindgen-macro-support, wasm-bindgen-shared, wasm-streams, web-sys,
web_atoms, weezl, widestring, windows, windows-collections, windows-core, windows-future,
windows-implement, windows-interface, windows-link, windows-numerics, windows-result,
windows-service, windows-strings, windows-sys, windows-targets, windows-threading,
windows-version, windows_aarch64_gnullvm, windows_aarch64_msvc, windows_i686_gnu,
windows_i686_gnullvm, windows_i686_msvc, windows_x86_64_gnu, windows_x86_64_gnullvm,
windows_x86_64_msvc. Also (equivalent, differently formatted `license` fields): fnv
(Apache-2.0/MIT), cesu8/dbus/libdbus-sys/rustc-hash (Apache-2.0/MIT), bitflags/bs58/
downcast-rs/foreign-types/foreign-types-macros/foreign-types-shared/ident_case/jni/
json-patch/minimal-lexical/unic-char-property/unic-char-range/unic-common/unic-ucd-ident/
unic-ucd-version/version_check/winapi/winapi-i686-pc-windows-gnu/
winapi-x86_64-pc-windows-gnu (MIT/Apache-2.0), rust-stemmers (MIT/BSD-3-Clause), dpi
(Apache-2.0 AND MIT), htmlescape (Apache-2.0/MIT/MPL-2.0), unicode-ident
((MIT OR Apache-2.0) AND Unicode-3.0).

**Apache-2.0:** sketches-ddsketch, sync_wrapper, tao, zopfli.

**Apache-2.0 OR MIT:** atomic-waker, autocfg, bit-set, bit-vec, cargo_toml, ctor,
ctor-proc-macro, drag, dtor, dtor-proc-macro, equivalent, fastrand, idna_adapter, indexmap,
libappindicator, libappindicator-sys, muda, pin-project-lite, portable-atomic,
portable-atomic-util, rustc-hash, tauri, tauri-build, tauri-codegen, tauri-macros,
tauri-runtime, tauri-runtime-wry, tauri-utils, utf8_iter, uuid, window-vibrancy, wry.

**Apache-2.0 WITH LLVM-exception (or, dual-licensed, plain Apache-2.0/MIT):**
target-lexicon; linux-raw-sys, rustix, wasi, wasip2, wit-bindgen.

**BSD (2- or 3-clause), plain or dual-licensed with MIT/Apache-2.0:** alloc-no-stdlib,
alloc-stdlib, instant, zstd-safe, zstd-sys (BSD-3-Clause); brotli (BSD-3-Clause AND MIT);
brotli-decompressor (BSD-3-Clause/MIT); moxcms, pxfm (BSD-3-Clause OR Apache-2.0); num_enum,
num_enum_derive (BSD-3-Clause OR MIT OR Apache-2.0); zerocopy, zerocopy-derive (BSD-2-Clause OR
Apache-2.0 OR MIT).

**ISC:** libloading.

**MPL-2.0:** cssparser, cssparser-macros, dtoa-short, option-ext, selectors. (Weak/file-level
copyleft: applies only to modifications of these crates' own files, which this project does
not modify — does not extend to code that merely links against them.)

**Unlicense, plain or dual-licensed with MIT:** aho-corasick, byteorder, byteorder-lite, jiff,
jiff-core, jiff-static, jiff-tzdb, jiff-tzdb-platform, memchr, winapi-util (Unlicense OR MIT);
same-file, tantivy-fst, utf8-ranges, walkdir (Unlicense/MIT).

**Zlib, plain or dual-licensed:** foldhash, zlib-rs (Zlib); bytemuck, dispatch2,
objc2-app-kit, objc2-cloud-kit, objc2-core-data, objc2-core-foundation, objc2-core-graphics,
objc2-core-image, objc2-core-location, objc2-core-text, objc2-core-video,
objc2-exception-helper, objc2-io-surface, objc2-quartz-core, objc2-ui-kit,
objc2-user-notifications, objc2-web-kit, tinyvec (Zlib OR Apache-2.0 OR MIT); fastdivide
(zlib-acknowledgement OR MIT).

**CC0-1.0, dual-licensed:** dunce (CC0-1.0 OR MIT-0 OR Apache-2.0).

**Unicode-3.0:** icu_collections, icu_locale_core, icu_normalizer, icu_normalizer_data,
icu_properties, icu_properties_data, icu_provider, litemap, potential_utf, tinystr, writeable,
yoke, yoke-derive, zerofrom, zerofrom-derive, zerotrie, zerovec, zerovec-derive.

**Other permissive dual/triple licenses:** r-efi (MIT OR Apache-2.0 OR LGPL-2.1-or-later — used
here under MIT or Apache-2.0, never the LGPL option); raw-window-handle (MIT OR Apache-2.0 OR
Zlib); miniz_oxide (MIT OR Zlib OR Apache-2.0); adler2 (0BSD OR MIT OR Apache-2.0).
