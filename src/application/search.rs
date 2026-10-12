//! Search options and the compiled query matcher.
//!
//! The query language mirrors voidtools Everything: space-separated terms are
//! AND-combined, `|` is OR, `!` negates the following term, `( … )` groups a
//! sub-expression (so `a (b | c)` is `a AND (b OR c)`), and `"quoted"` text is
//! a literal phrase (escaping the operators — including parentheses). `*`/`?`
//! wildcards,
//! whole-word and regex modes, case sensitivity, full-path matching, and the
//! `ext:`, `path:`, `file:`, `folder:`, `size:`, `dm:`/`dc:`/`da:` (modified /
//! created / accessed dates) and `attrib:` functions are all supported as leaf
//! predicates, so e.g. `ext:dll | ext:exe`, `report !draft`, `"my file"
//! size:>1mb`, `dm:today`, `dc:2024-01-01..2024-06-30` and `attrib:h` all work.

use chrono::{Datelike, Days, Local, Months, NaiveDate, TimeZone};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

/// Which column results are ordered by.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    Created,
    Accessed,
    Ext,
    Attributes,
}

/// The entry fields a [`Matcher`] inspects. `path` / `path_lower` may be empty
/// strings when [`Matcher::needs_path`] is false. Borrowed, so building one per
/// candidate entry allocates nothing.
pub struct EntryView<'a> {
    pub name: &'a str,
    pub name_lower: &'a str,
    pub path: &'a str,
    pub path_lower: &'a str,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified_ms: Option<i64>,
    pub created_ms: Option<i64>,
    pub accessed_ms: Option<i64>,
    pub attributes: u32,
}

/// Everything-style search options sent from the UI (or a service client).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchOptions {
    pub query: String,
    pub limit: usize,
    /// Skip this many ranked hits first (paging through a huge result set).
    #[serde(default)]
    pub offset: usize,
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
    /// Ignore accents/diacritics (`é` matches `e`) in name/path text matching.
    pub diacritics: bool,
    pub match_path: bool,
    pub sort: SortKey,
    pub ascending: bool,
    /// Group directories ahead of files, regardless of the sort column/direction.
    pub folders_first: bool,
    /// List hidden files too (protected operating-system files, hidden + system, never show).
    pub show_hidden: bool,
    pub show_system: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: 5000,
            offset: 0,
            match_case: false,
            whole_word: false,
            regex: false,
            diacritics: false,
            match_path: false,
            sort: SortKey::Name,
            ascending: true,
            folders_first: false,
            show_hidden: false,
            show_system: false,
        }
    }
}

/// Inclusive byte-size bounds from a `size:` operator.
#[derive(Default)]
struct SizeFilter {
    min: Option<u64>,
    max: Option<u64>,
}

/// Which timestamp a date predicate tests.
#[derive(Clone, Copy)]
enum DateField {
    Modified,
    Created,
    Accessed,
}

/// Inclusive Unix-millisecond bounds from a `dm:`/`dc:`/`da:` operator.
struct DateFilter {
    min: Option<i64>,
    max: Option<i64>,
}

/// Whether a text predicate matches the file name or the full path.
#[derive(Clone, Copy)]
enum TextTarget {
    Name,
    Path,
}

enum TextKind {
    /// Substring; pre-cased to match the haystack casing. Searched with a prebuilt SIMD finder, not `str::contains`
    /// (which sets its searcher up again for every one of millions of names).
    Plain(memchr::memmem::Finder<'static>),
    /// Compiled wildcard / whole-word / regex pattern. `on_lower` matches the
    /// pre-folded lowercase haystack (wildcard/whole-word built from lowercased
    /// literals, so case folding is identical to `Plain`); otherwise it matches
    /// the original-case haystack (user regex mode, or case-sensitive search).
    Re { re: Regex, on_lower: bool },
}

/// A single test against one entry.
enum Pred {
    Text {
        target: TextTarget,
        kind: TextKind,
    },
    Ext(Vec<String>),
    Size(SizeFilter),
    Date {
        field: DateField,
        filter: DateFilter,
    },
    /// All bits in the mask must be set in the entry's attributes.
    Attrib(u32),
    File,
    Folder,
    /// The item's own path is the immediate containing folder of `value` (so
    /// `parent:C:\a\b\c.txt` finds the single folder `C:\a\b`). `value` is
    /// pre-computed to the parent directory at parse time.
    Parent(String),
    /// The item is a direct child of the folder `value` (`child:C:\a\b` lists
    /// what's directly inside it, one level deep).
    Child(String),
    /// The item shares `value`'s immediate parent folder, and isn't `value` itself.
    Sibling(String),
    /// The item's path is a folder somewhere above `value` in the tree (a proper
    /// prefix directory of it).
    Ancestor(String),
    /// The item's path is anywhere under the folder `value`, at any depth.
    Descendant(String),
    /// A directory with nothing in it. Unlike every other predicate (index-only),
    /// this is a real filesystem syscall per candidate.
    Empty,
    /// The item's path still exists on disk right now. A real filesystem syscall
    /// per candidate, same perf caveat as `Empty` — cheap for a handful of
    /// results, a real cost if it runs over a huge candidate set.
    Exists,
    /// Matches nothing. Produced when a recognised filter (`size:`/`dm:`/…) has a
    /// non-empty but unparseable value, so the query returns zero results instead
    /// of silently dropping the filter and matching the entire index.
    Never,
}

/// The directory portion of a Windows path (everything before the final `\`), or "" when there
/// isn't one.
fn parent_dir(path: &str) -> &str {
    path.rsplit_once('\\').map_or("", |(dir, _)| dir)
}

/// A node in the compiled boolean expression tree. Leaves are predicates;
/// interior nodes combine them with AND / OR / NOT. Parenthesised groups in the
/// query nest as further `And`/`Or` nodes.
enum Expr {
    Pred(Pred),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

/// A compiled query: a boolean expression tree, or `None` when nothing
/// constrains the result set (an empty query matches everything).
pub struct Matcher {
    root: Option<Expr>,
    match_case: bool,
    diacritics: bool,
    needs_path: bool,
}

/// NFD-decompose and drop combining marks, so `é`/`e`, `ñ`/`n`, etc. compare equal.
fn strip_diacritics(s: &str) -> String {
    use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
    s.nfd().filter(|c| !is_combining_mark(*c)).collect()
}

enum Token {
    Word(String),
    Phrase(String),
    Or,
    Not,
    LParen,
    RParen,
}

impl Matcher {
    /// Compile search options into a matcher, or return a user-facing error
    /// (e.g. an invalid regex).
    pub fn compile(opts: &SearchOptions) -> Result<Self, String> {
        let case = opts.match_case;
        let diacritics = opts.diacritics;

        // Regex mode treats the whole query as one expression.
        if opts.regex {
            let query = opts.query.trim();
            if query.is_empty() {
                return Ok(Self {
                    root: None,
                    match_case: case,
                    diacritics,
                    needs_path: false,
                });
            }
            let target = if opts.match_path {
                TextTarget::Path
            } else {
                TextTarget::Name
            };
            let pred = Pred::Text {
                target,
                // User regex matches the original-case haystack (the regex crate's
                // own case folding via `case_insensitive`).
                kind: TextKind::Re {
                    re: build_regex(query, case)?,
                    on_lower: false,
                },
            };
            return Ok(Self {
                root: Some(Expr::Pred(pred)),
                match_case: case,
                diacritics,
                needs_path: opts.match_path,
            });
        }

        let tokens = tokenize(&opts.query);
        let mut parser = Parser {
            tokens: &tokens,
            pos: 0,
            opts,
            needs_path: false,
        };
        let root = parser.parse_or(0)?;

        Ok(Self {
            root,
            match_case: case,
            diacritics,
            needs_path: parser.needs_path,
        })
    }

    /// True when nothing constrains the result set (so every entry matches).
    pub fn matches_all(&self) -> bool {
        self.root.is_none()
    }

    /// The folder of a plain `child:` query (nothing else in it): the index can answer that from parent numbers
    /// without rebuilding every entry's path.
    pub fn only_child(&self) -> Option<&str> {
        match &self.root {
            Some(Expr::Pred(Pred::Child(t))) => Some(t),
            _ => None,
        }
    }

    /// The folders of a query that is only `parent:` terms OR-ed together (Home): the index finds them by walking
    /// the tree instead of rebuilding every entry's path.
    pub fn only_parents(&self) -> Option<Vec<&str>> {
        match &self.root {
            Some(Expr::Pred(Pred::Parent(t))) => Some(vec![t]),
            Some(Expr::Or(parts)) => parts.iter().map(|p| if let Expr::Pred(Pred::Parent(t)) = p { Some(t.as_str()) } else { None }).collect(),
            _ => None,
        }
    }

    /// Whether matching requires the reconstructed full path.
    pub fn needs_path(&self) -> bool {
        self.needs_path
    }

    /// Whether matching reads original-case text (`EntryView::name`/`path`); otherwise only the lowercased fields.
    pub fn match_case(&self) -> bool {
        self.match_case
    }

    /// Test one entry. `e.path`/`e.path_lower` may be empty when `needs_path()`
    /// is false (no predicate looks at the path).
    pub fn eval(&self, e: &EntryView) -> bool {
        match &self.root {
            None => true,
            Some(expr) => self.eval_expr(expr, e),
        }
    }

    fn eval_expr(&self, expr: &Expr, e: &EntryView) -> bool {
        match expr {
            Expr::Pred(pred) => self.eval_pred(pred, e),
            Expr::Not(inner) => !self.eval_expr(inner, e),
            Expr::And(parts) => parts.iter().all(|x| self.eval_expr(x, e)),
            Expr::Or(parts) => parts.iter().any(|x| self.eval_expr(x, e)),
        }
    }

    fn eval_pred(&self, pred: &Pred, e: &EntryView) -> bool {
        match pred {
            Pred::File => !e.is_dir,
            Pred::Folder => e.is_dir,
            Pred::Ext(list) => ext_allows(e.name_lower, list),
            Pred::Size(filter) => !e.is_dir && size_within(e.size, filter),
            Pred::Date { field, filter } => {
                let value = match field {
                    DateField::Modified => e.modified_ms,
                    DateField::Created => e.created_ms,
                    DateField::Accessed => e.accessed_ms,
                };
                date_within(value, filter)
            }
            Pred::Attrib(mask) => e.attributes & mask == *mask,
            Pred::Parent(target) | Pred::Child(target) | Pred::Sibling(target) | Pred::Ancestor(target) | Pred::Descendant(target) => {
                let path = if self.match_case { e.path } else { e.path_lower };
                match pred {
                    Pred::Parent(_) => path == target,
                    Pred::Child(_) => parent_dir(path) == target,
                    Pred::Sibling(_) => path != target && parent_dir(path) == parent_dir(target),
                    Pred::Ancestor(_) => {
                        target.len() > path.len() && target.starts_with(path) && target.as_bytes()[path.len()] == b'\\'
                    }
                    _ => path.len() > target.len() && path.starts_with(target.as_str()) && path.as_bytes()[target.len()] == b'\\',
                }
            }
            Pred::Empty => e.is_dir && std::fs::read_dir(e.path).map(|mut d| d.next().is_none()).unwrap_or(false),
            Pred::Exists => std::path::Path::new(e.path).exists(),
            Pred::Never => false,
            Pred::Text { target, kind } => {
                let (orig, lower) = match target {
                    TextTarget::Name => (e.name, e.name_lower),
                    TextTarget::Path => (e.path, e.path_lower),
                };
                // Diacritics-insensitive mode strips accents from the needle at compile
                // time (see `text_pred`/`phrase_pred`); the haystack is stripped here,
                // per candidate, only when the toggle is actually on.
                let stripped;
                let (orig, lower) = if self.diacritics {
                    stripped = (strip_diacritics(orig), strip_diacritics(lower));
                    (stripped.0.as_str(), stripped.1.as_str())
                } else {
                    (orig, lower)
                };
                match kind {
                    TextKind::Plain(finder) => finder.find(if self.match_case { orig } else { lower }.as_bytes()).is_some(),
                    TextKind::Re { re, on_lower } => {
                        re.is_match(if *on_lower { lower } else { orig })
                    }
                }
            }
        }
    }
}



/// Pull a `distinct:field` / `unique:field` out of `query` (`field` is `name`, `ext`, or `size`),
/// returning the remaining query and the field to dedupe results by. The last occurrence wins if
/// repeated; an unrecognised field is dropped, like an unparseable `size:`/`dm:` elsewhere.
///
/// Distinctness is a post-ranking aggregation over the whole result set, not a per-entry test, so
/// (unlike every other function here) it isn't a `Pred` — the caller applies it after `Matcher`
/// produces and ranks the matching entries.
pub fn extract_distinct(query: &str) -> (String, Option<String>) {
    let mut field = None;
    let mut rest = Vec::new();
    for word in query.split_whitespace() {
        match strip_prefix_ci(word, "distinct:").or_else(|| strip_prefix_ci(word, "unique:")) {
            Some(v) if matches!(v, "name" | "ext" | "size") => field = Some(v.to_string()),
            Some(_) => {} // unrecognised field: drop the operator
            None => rest.push(word),
        }
    }
    (rest.join(" "), field)
}

/// Split a query into words, quoted phrases, and the `|`/`!`/`(`/`)` operators.
/// `|` and `"` cannot appear in NTFS file names, so they are always operators;
/// `!` is the NOT operator only at the start of a term (so names like
/// `!Locales` are still searchable as substrings, and `"!x"` matches one
/// literally). Parentheses *can* appear in file names (`Program Files (x86)`),
/// so — matching Everything — unquoted `(`/`)` are always grouping operators;
/// to match a literal parenthesis, quote it (`"(x86)"`).
fn tokenize(query: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut chars = query.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                flush_word(&mut buf, &mut tokens);
                let mut phrase = String::new();
                for n in chars.by_ref() {
                    if n == '"' {
                        break;
                    }
                    phrase.push(n);
                }
                if !phrase.is_empty() {
                    tokens.push(Token::Phrase(phrase));
                }
            }
            '|' => {
                flush_word(&mut buf, &mut tokens);
                tokens.push(Token::Or);
            }
            '(' => {
                flush_word(&mut buf, &mut tokens);
                tokens.push(Token::LParen);
            }
            ')' => {
                flush_word(&mut buf, &mut tokens);
                tokens.push(Token::RParen);
            }
            '!' if buf.is_empty() => tokens.push(Token::Not),
            c if c.is_whitespace() => flush_word(&mut buf, &mut tokens),
            _ => buf.push(c),
        }
    }
    flush_word(&mut buf, &mut tokens);
    tokens
}

/// Recursive-descent parser over [`Token`]s into an [`Expr`] tree. The grammar,
/// lowest precedence first:
///
/// ```text
/// or      := and ( '|' and )*
/// and     := unary*                 // implicit AND of space-separated terms
/// unary   := '!' unary | primary
/// primary := '(' or ')' | word | phrase
/// ```
///
/// It is deliberately lenient: unbalanced parentheses never error — a stray
/// `)` is skipped and an unclosed `(` is implicitly closed at end of input —
/// so a half-typed query still returns useful results instead of nothing.
/// Maximum parenthesis nesting depth the parser will recurse into. Real queries
/// never approach this; it only bounds stack growth on pathological input.
const MAX_GROUP_DEPTH: u32 = 64;

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    opts: &'a SearchOptions,
    needs_path: bool,
}

impl Parser<'_> {
    /// `or := and ( '|' and )*`
    fn parse_or(&mut self, depth: u32) -> Result<Option<Expr>, String> {
        let mut alts = Vec::new();
        if let Some(node) = self.parse_and(depth)? {
            alts.push(node);
        }
        while matches!(self.tokens.get(self.pos), Some(Token::Or)) {
            self.pos += 1; // consume '|'
            if let Some(node) = self.parse_and(depth)? {
                alts.push(node);
            }
        }
        Ok(combine(alts, true))
    }

    /// `and := unary*` — implicit AND of terms, ending at `|`, a closing `)`
    /// (when inside a group), or end of input.
    fn parse_and(&mut self, depth: u32) -> Result<Option<Expr>, String> {
        let mut parts = Vec::new();
        loop {
            match self.tokens.get(self.pos) {
                None | Some(Token::Or) => break,
                Some(Token::RParen) if depth > 0 => break,
                // A `)` with no open group is junk — skip it and continue.
                Some(Token::RParen) => {
                    self.pos += 1;
                    continue;
                }
                _ => {}
            }
            if let Some(node) = self.parse_unary(depth)? {
                parts.push(node);
            }
        }
        Ok(combine(parts, false))
    }

    /// `unary := '!'* primary`
    ///
    /// A run of `!` is folded *iteratively* into a single parity bit rather than
    /// recursing per `!` (`!!x` ≡ `x`, `!!!x` ≡ `!x`). This is both correct
    /// boolean double-negation and essential for safety: a recursive `'!' unary`
    /// rule would grow the stack one frame per `!`, and a long run of `!` (which
    /// the cap on `(` does not touch) would overflow it — fatal under
    /// `panic = "abort"`. Folding keeps `!` depth O(1), so the only source of
    /// tree depth is parenthesis nesting, already bounded by `MAX_GROUP_DEPTH`.
    fn parse_unary(&mut self, depth: u32) -> Result<Option<Expr>, String> {
        let mut negate = false;
        while matches!(self.tokens.get(self.pos), Some(Token::Not)) {
            self.pos += 1; // consume '!'
            negate = !negate;
        }
        let inner = self.parse_primary(depth)?;
        Ok(inner.map(|e| if negate { Expr::Not(Box::new(e)) } else { e }))
    }

    /// `primary := '(' or ')' | word | phrase`
    fn parse_primary(&mut self, depth: u32) -> Result<Option<Expr>, String> {
        match self.tokens.get(self.pos) {
            Some(Token::LParen) => {
                self.pos += 1; // consume '('
                               // Past the nesting cap, stop recursing: drop this `(` and let
                               // the current level absorb its contents (its `)` becomes a stray
                               // that's skipped). A run of `((((…` can't overflow the stack —
                               // and with `panic = "abort"` a stack overflow would kill the app.
                if depth >= MAX_GROUP_DEPTH {
                    return Ok(None);
                }
                let inner = self.parse_or(depth + 1)?;
                if matches!(self.tokens.get(self.pos), Some(Token::RParen)) {
                    self.pos += 1; // consume matching ')'
                }
                Ok(inner)
            }
            Some(Token::Word(word)) => {
                let mut word = word.clone();
                self.pos += 1;
                // `child:"C:\Program Files"`: the tokenizer splits at the quote, so a relation operator left
                // without a value takes the quoted phrase right after it (paths with spaces)
                if RELATIONS.iter().any(|r| word.eq_ignore_ascii_case(r)) {
                    if let Some(Token::Phrase(path)) = self.tokens.get(self.pos) {
                        word.push_str(path);
                        self.pos += 1;
                    }
                }
                Ok(parse_word(&word, self.opts, &mut self.needs_path)?.map(Expr::Pred))
            }
            Some(Token::Phrase(phrase)) => {
                let phrase = phrase.clone();
                self.pos += 1;
                Ok(phrase_pred(&phrase, self.opts, &mut self.needs_path).map(Expr::Pred))
            }
            // A structural token (`)`, `|`, or end) reached where a term was
            // expected — e.g. a trailing `!`. Yield nothing without consuming so
            // the enclosing rule can handle it.
            _ => Ok(None),
        }
    }
}

/// Fold parsed parts into one node: drop when empty (no constraint), unwrap a
/// lone child, otherwise wrap as `Or` (`is_or`) or `And`.
fn combine(mut parts: Vec<Expr>, is_or: bool) -> Option<Expr> {
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(if is_or {
            Expr::Or(parts)
        } else {
            Expr::And(parts)
        }),
    }
}

fn flush_word(buf: &mut String, tokens: &mut Vec<Token>) {
    if !buf.is_empty() {
        tokens.push(Token::Word(std::mem::take(buf)));
    }
}

/// Case-insensitively strip an ASCII operator prefix (`size:`, `ext:`, …),
/// returning the remainder. The prefixes are all ASCII, so `Size:`/`EXT:` are
/// treated the same as `size:`/`ext:` — consistent with `file:`/`folder:`.
fn strip_prefix_ci<'a>(word: &'a str, prefix: &str) -> Option<&'a str> {
    let head = word.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &word[prefix.len()..])
}

/// Resolve a recognised filter (`size:`/`dm:`/`ext:`/…) into a predicate. An
/// empty value is dropped (the user is mid-typing the operator). A non-empty but
/// unparseable value (`size:gb`, `dm:2024-02-30`) yields a never-matching
/// predicate, so the query returns zero results rather than silently dropping
/// the filter and matching the entire index.
fn filter_pred(value: &str, parsed: Option<Pred>) -> Option<Pred> {
    match parsed {
        Some(p) => Some(p),
        None if value.trim().is_empty() => None,
        None => Some(Pred::Never),
    }
}

const RELATIONS: [&str; 5] = ["parent:", "child:", "sibling:", "ancestor:", "descendant:"];

/// Parse a bare word into a predicate: a function (`ext:`/`size:`/`file:`/
/// `folder:`/`path:`) or a text match.
fn parse_word(
    word: &str,
    opts: &SearchOptions,
    needs_path: &mut bool,
) -> Result<Option<Pred>, String> {
    if word.eq_ignore_ascii_case("file:") || word.eq_ignore_ascii_case("files:") {
        return Ok(Some(Pred::File));
    }
    if word.eq_ignore_ascii_case("folder:")
        || word.eq_ignore_ascii_case("folders:")
        || word.eq_ignore_ascii_case("dir:")
    {
        return Ok(Some(Pred::Folder));
    }
    if let Some(v) = strip_prefix_ci(word, "ext:") {
        // Fold with Unicode `to_lowercase` to match the index's `name_lower`
        // (the haystack is folded the same way); `to_ascii_lowercase` would miss
        // non-ASCII extensions. A leading dot (`ext:.dll`) is tolerated.
        let exts: Vec<String> = v
            .split([';', ','])
            .filter(|e| !e.is_empty())
            .map(|e| e.strip_prefix('.').unwrap_or(e).to_lowercase())
            .collect();
        return Ok(filter_pred(
            v,
            (!exts.is_empty()).then_some(Pred::Ext(exts)),
        ));
    }
    if let Some(v) = strip_prefix_ci(word, "size:") {
        return Ok(filter_pred(v, parse_size_filter(v).map(Pred::Size)));
    }
    if let Some(v) = strip_prefix_ci(word, "dm:") {
        return Ok(filter_pred(v, date_pred(v, DateField::Modified)));
    }
    if let Some(v) = strip_prefix_ci(word, "dc:") {
        return Ok(filter_pred(v, date_pred(v, DateField::Created)));
    }
    if let Some(v) = strip_prefix_ci(word, "da:") {
        return Ok(filter_pred(v, date_pred(v, DateField::Accessed)));
    }
    if let Some(v) = strip_prefix_ci(word, "attrib:") {
        return Ok(filter_pred(v, parse_attrib(v).map(Pred::Attrib)));
    }
    if let Some(v) = strip_prefix_ci(word, "path:") {
        *needs_path = true;
        return Ok(text_pred(v, TextTarget::Path, opts));
    }
    if word.eq_ignore_ascii_case("empty:") {
        *needs_path = true;
        return Ok(Some(Pred::Empty));
    }
    if word.eq_ignore_ascii_case("exists:") {
        *needs_path = true;
        return Ok(Some(Pred::Exists));
    }
    for (prefix, wrap) in RELATIONS.into_iter().zip([Pred::Parent as fn(String) -> Pred, Pred::Child, Pred::Sibling, Pred::Ancestor, Pred::Descendant]) {
        if let Some(v) = strip_prefix_ci(word, prefix) {
            if v.trim().is_empty() {
                return Ok(None);
            }
            *needs_path = true;
            let target = path_arg(v, opts);
            let target = if matches!(prefix, "parent:") { parent_dir(&target).to_string() } else { target };
            return Ok(Some(wrap(target)));
        }
    }

    // A bare path-like word — one containing a `\` separator or starting with a
    // drive letter (`C:\Users\…`, `Downloads\report`, `C:`) — matches the full
    // path, mirroring Everything: typing a folder path lists what's under it.
    let target = if opts.match_path || looks_like_path(word) {
        *needs_path = true;
        TextTarget::Path
    } else {
        TextTarget::Name
    };
    Ok(text_pred(word, target, opts))
}

/// Heuristic: does a bare query word look like a filesystem path? True when it
/// contains a `\` path separator or begins with a drive-letter prefix (`C:`).
/// Such words are matched against the full path rather than the name alone.
fn looks_like_path(word: &str) -> bool {
    if word.contains('\\') {
        return true;
    }
    let mut chars = word.chars();
    matches!((chars.next(), chars.next()), (Some(c), Some(':')) if c.is_ascii_alphabetic())
}

/// Case-fold (per `opts.match_case`) and trim a trailing separator off a `parent:`/`child:`/
/// `sibling:`/`ancestor:`/`descendant:` argument, the same convention as every other text needle.
fn path_arg(v: &str, opts: &SearchOptions) -> String {
    let v = v.trim_end_matches('\\');
    if opts.match_case { v.to_string() } else { v.to_lowercase() }
}

/// A quoted phrase is always a literal substring (no functions or wildcards). A
/// quoted path matches the full path, the same as an unquoted one.
fn phrase_pred(phrase: &str, opts: &SearchOptions, needs_path: &mut bool) -> Option<Pred> {
    let target = if opts.match_path || looks_like_path(phrase) {
        *needs_path = true;
        TextTarget::Path
    } else {
        TextTarget::Name
    };
    let mut needle = if opts.match_case {
        phrase.to_string()
    } else {
        phrase.to_lowercase()
    };
    if opts.diacritics {
        needle = strip_diacritics(&needle);
    }
    Some(Pred::Text {
        target,
        kind: plain(needle),
    })
}

/// Build a text predicate from `text`, honouring wildcards, whole-word and case.
fn text_pred(text: &str, target: TextTarget, opts: &SearchOptions) -> Option<Pred> {
    if text.is_empty() {
        return None;
    }
    // Wildcards take precedence over whole-word: a glob is already anchored to the
    // whole name, so "whole word" adds nothing meaningful to an open-ended pattern.
    let has_glob = text.contains('*') || text.contains('?');
    if has_glob || opts.whole_word {
        // Build the pattern with the SAME case folding as the Plain path: when
        // case-insensitive, lowercase the literals and match the pre-folded
        // lowercase haystack (`name_lower`/`path_lower`). This keeps results
        // identical whether or not a query carries a wildcard / whole-word, even
        // for non-ASCII characters where `to_lowercase` and the regex crate's
        // case folding disagree.
        let on_lower = !opts.match_case;
        let mut src = if on_lower {
            text.to_lowercase()
        } else {
            text.to_string()
        };
        if opts.diacritics {
            src = strip_diacritics(&src);
        }
        let pattern = if has_glob {
            glob_pattern(&src, glob_anchored(&src, target))
        } else {
            word_pattern(&src)
        };
        // Pattern literals are already correctly cased, so the regex itself is
        // case-sensitive (`match_case = true`).
        return match build_regex(&pattern, true) {
            Ok(re) => Some(Pred::Text {
                target,
                kind: TextKind::Re { re, on_lower },
            }),
            Err(_) => None,
        };
    }
    let mut needle = if opts.match_case { text.to_string() } else { text.to_lowercase() };
    if opts.diacritics {
        needle = strip_diacritics(&needle);
    }
    Some(Pred::Text { target, kind: plain(needle) })
}

fn plain(needle: String) -> TextKind {
    TextKind::Plain(memchr::memmem::Finder::new(needle.as_bytes()).into_owned())
}

fn ext_allows(name_lower: &str, exts: &[String]) -> bool {
    match name_lower.rfind('.') {
        Some(i) => {
            let ext = &name_lower[i + 1..];
            exts.iter().any(|e| e == ext)
        }
        None => false,
    }
}

fn size_within(size: Option<u64>, filter: &SizeFilter) -> bool {
    // An unknown size must not masquerade as 0 — that would spuriously match
    // `size:0`/`size:<N` and be excluded from `size:>0`. A size filter always
    // carries at least one bound, so an unknown size satisfies none of them.
    let Some(bytes) = size else {
        return filter.min.is_none() && filter.max.is_none();
    };
    if filter.min.is_some_and(|m| bytes < m) {
        return false;
    }
    if filter.max.is_some_and(|m| bytes > m) {
        return false;
    }
    true
}

fn build_regex(pattern: &str, match_case: bool) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(!match_case)
        .size_limit(1 << 22)
        .build()
        .map_err(|e| e.to_string())
}

/// Whether a wildcard query should anchor to the whole haystack. A name glob
/// always does (`*.pdf` means the whole name). A path glob only anchors when the
/// fragment is rooted at the drive (`C:\…`) or a separator; an interior fragment
/// (`Downloads\*.pdf`) matches anywhere in the path, mirroring the substring
/// semantics the same fragment has without a wildcard.
fn glob_anchored(text: &str, target: TextTarget) -> bool {
    match target {
        TextTarget::Name => true,
        TextTarget::Path => {
            text.starts_with('\\') || {
                let mut c = text.chars();
                matches!((c.next(), c.next()), (Some(a), Some(':')) if a.is_ascii_alphabetic())
            }
        }
    }
}

/// Translate a `*`/`**`/`?` wildcard token into a regex, anchored `^…$` when
/// `anchored` (whole-haystack match) or unanchored (substring match). A single
/// `*` stays within one path segment (doesn't cross `\`), matching Everything's
/// own semantics; `**` crosses separators, matching any run of characters.
fn glob_pattern(glob: &str, anchored: bool) -> String {
    let mut p = String::new();
    if anchored {
        p.push('^');
    }
    let mut literal = String::new();
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '*' || c == '?' {
            if !literal.is_empty() {
                p.push_str(&regex::escape(&literal));
                literal.clear();
            }
            if c == '*' && chars.get(i + 1) == Some(&'*') {
                p.push_str(".*");
                i += 2;
                continue;
            }
            p.push_str(if c == '*' { r"[^\\]*" } else { "." });
            i += 1;
            continue;
        }
        literal.push(c);
        i += 1;
    }
    if !literal.is_empty() {
        p.push_str(&regex::escape(&literal));
    }
    if anchored {
        p.push('$');
    }
    p
}

/// Whole-word pattern. `\b` alone only fires at a word/non-word transition, so a
/// token that begins or ends with a non-word character (`.gitignore`, `C++`,
/// `.log`) could never satisfy a bare `\b` at that edge. For a non-word edge use
/// `(?:^|\b|\W)` / `(?:\b|\W|$)`: `\b` matches a word→non-word boundary (so `.log`
/// matches `system.log`), `\W` matches a non-word→non-word boundary (`a .log`),
/// and `^`/`$` match the whole-name case (`.gitignore`). The regex crate has no
/// lookaround, so `\W` consumes the boundary char — fine here, since the
/// predicate only uses `is_match` (a boolean), never the match span.
fn word_pattern(token: &str) -> String {
    fn is_word(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }
    let esc = regex::escape(token);
    let lead = match token.chars().next() {
        Some(c) if is_word(c) => r"\b",
        _ => r"(?:^|\b|\W)",
    };
    let trail = match token.chars().next_back() {
        Some(c) if is_word(c) => r"\b",
        _ => r"(?:\b|\W|$)",
    };
    format!("{lead}{esc}{trail}")
}

/// Parse a `size:` value such as `>100mb`, `<=1gb`, `>=512`, `4kb`, or an
/// inclusive `A..B` range (one side may be empty: `..1mb`, `1mb..`).
fn parse_size_filter(value: &str) -> Option<SizeFilter> {
    // Inclusive range, mirroring the date filter: each side optional, reversed
    // endpoints tolerated. A non-empty but unparseable side returns None (→ a
    // never-match filter upstream).
    if let Some((a, b)) = value.split_once("..") {
        let lo = if a.trim().is_empty() {
            None
        } else {
            Some(parse_size_bytes(a)?)
        };
        let hi = if b.trim().is_empty() {
            None
        } else {
            Some(parse_size_bytes(b)?)
        };
        return match (lo, hi) {
            (Some(lo), Some(hi)) => Some(SizeFilter {
                min: Some(lo.min(hi)),
                max: Some(lo.max(hi)),
            }),
            (Some(lo), None) => Some(SizeFilter {
                min: Some(lo),
                max: None,
            }),
            (None, Some(hi)) => Some(SizeFilter {
                min: None,
                max: Some(hi),
            }),
            (None, None) => None,
        };
    }

    let (op, rest) = if let Some(r) = value.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = value.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = value.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = value.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = value.strip_prefix('=') {
        // Explicit equals (`size:=0`, `size:=8mb`); the date parser accepts it too.
        ("=", r)
    } else {
        ("=", value)
    };

    let bytes = parse_size_bytes(rest)?;
    Some(match op {
        ">" => SizeFilter {
            min: Some(bytes.saturating_add(1)),
            max: None,
        },
        ">=" => SizeFilter {
            min: Some(bytes),
            max: None,
        },
        // Nothing is smaller than 0 bytes — `size:<0` must match nothing, not
        // collapse to `<=0` (matching empty files) via saturating_sub.
        "<" if bytes == 0 => SizeFilter {
            min: Some(1),
            max: Some(0),
        },
        "<" => SizeFilter {
            min: None,
            max: Some(bytes - 1),
        },
        "<=" => SizeFilter {
            min: None,
            max: Some(bytes),
        },
        _ => SizeFilter {
            min: Some(bytes),
            max: Some(bytes),
        },
    })
}

/// Parse a number with an optional unit (`kb`/`mb`/`gb`/`tb`, 1024-based).
fn parse_size_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.parse().ok()?;
    let multiplier: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" | "k" => 1024.0,
        "mb" | "m" => 1024.0 * 1024.0,
        "gb" | "g" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "t" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * multiplier) as u64)
}

/// Build a date predicate, or `None` if the value is unparseable.
fn date_pred(value: &str, field: DateField) -> Option<Pred> {
    parse_date_filter(value).map(|filter| Pred::Date { field, filter })
}

/// Parse a `dm:`/`dc:`/`da:` value into inclusive millisecond bounds. Accepts a
/// keyword (`today`, `yesterday`, `thisweek`/`lastweek`, `thismonth`/
/// `lastmonth`, `thisyear`/`lastyear`), an absolute `YYYY`, `YYYY-MM` or
/// `YYYY-MM-DD` (`/` also allowed), each optionally prefixed by `>`/`>=`/`<`/
/// `<=`/`=`, or an inclusive `A..B` range.
fn parse_date_filter(value: &str) -> Option<DateFilter> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some((a, b)) = value.split_once("..") {
        // Each side may be empty (open-ended: `..B` => up to B, `A..` => from A).
        // A non-empty but unparseable side returns None (→ a never-match filter
        // upstream), not a dropped predicate. A closed range tolerates reversed
        // endpoints by taking the union span.
        let span_a = if a.trim().is_empty() {
            None
        } else {
            Some(parse_date_span(a)?)
        };
        let span_b = if b.trim().is_empty() {
            None
        } else {
            Some(parse_date_span(b)?)
        };
        return match (span_a, span_b) {
            (Some((sa, ea)), Some((sb, eb))) => Some(DateFilter {
                min: Some(sa.min(sb)),
                max: Some(ea.max(eb) - 1),
            }),
            (Some((sa, _)), None) => Some(DateFilter {
                min: Some(sa),
                max: None,
            }),
            (None, Some((_, eb))) => Some(DateFilter {
                min: None,
                max: Some(eb - 1),
            }),
            (None, None) => None,
        };
    }
    let (op, rest) = split_date_op(value);
    let (start, end) = parse_date_span(rest)?;
    Some(match op {
        ">" => DateFilter {
            min: Some(end),
            max: None,
        },
        ">=" => DateFilter {
            min: Some(start),
            max: None,
        },
        "<" => DateFilter {
            min: None,
            max: Some(start - 1),
        },
        "<=" => DateFilter {
            min: None,
            max: Some(end - 1),
        },
        _ => DateFilter {
            min: Some(start),
            max: Some(end - 1),
        },
    })
}

fn split_date_op(value: &str) -> (&str, &str) {
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = value.strip_prefix(op) {
            return (op, rest);
        }
    }
    ("", value)
}

/// Resolve a date keyword or absolute date into a half-open local-time span
/// `[start, end)` in Unix milliseconds.
fn parse_date_span(s: &str) -> Option<(i64, i64)> {
    let s = s.trim().to_ascii_lowercase();
    let today = Local::now().date_naive();

    match s.as_str() {
        "today" => return span_days(today, today.checked_add_days(Days::new(1))?),
        "yesterday" => return span_days(today.checked_sub_days(Days::new(1))?, today),
        "thisweek" | "lastweek" => {
            let monday =
                today.checked_sub_days(Days::new(today.weekday().num_days_from_monday() as u64))?;
            return if s == "thisweek" {
                span_days(monday, monday.checked_add_days(Days::new(7))?)
            } else {
                span_days(monday.checked_sub_days(Days::new(7))?, monday)
            };
        }
        "thismonth" | "lastmonth" => {
            let first = today.with_day(1)?;
            return if s == "thismonth" {
                span_days(first, first.checked_add_months(Months::new(1))?)
            } else {
                span_days(first.checked_sub_months(Months::new(1))?, first)
            };
        }
        "thisyear" | "lastyear" => {
            let jan1 = NaiveDate::from_ymd_opt(today.year(), 1, 1)?;
            return if s == "thisyear" {
                span_days(jan1, NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)?)
            } else {
                span_days(NaiveDate::from_ymd_opt(today.year() - 1, 1, 1)?, jan1)
            };
        }
        _ => {}
    }

    let parts: Vec<&str> = s.split(['-', '/']).collect();
    match parts.as_slice() {
        [y] => {
            let year: i32 = y.parse().ok()?;
            span_days(
                NaiveDate::from_ymd_opt(year, 1, 1)?,
                NaiveDate::from_ymd_opt(year.checked_add(1)?, 1, 1)?,
            )
        }
        [y, m] => {
            let first = NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, 1)?;
            span_days(first, first.checked_add_months(Months::new(1))?)
        }
        [y, m, d] => {
            let date = NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)?;
            span_days(date, date.checked_add_days(Days::new(1))?)
        }
        _ => None,
    }
}

/// Convert a `[start, end)` date range (end exclusive) to Unix milliseconds at
/// local midnight.
fn span_days(start: NaiveDate, end: NaiveDate) -> Option<(i64, i64)> {
    Some((local_midnight_ms(start)?, local_midnight_ms(end)?))
}

/// Unix milliseconds at the start of `date` in local time. Normally local
/// midnight; on a spring-forward day where the clock jumps at exactly 00:00 (a
/// few zones, e.g. America/Sao_Paulo historically) midnight doesn't exist, so
/// advance to the first valid wall-clock time of the day rather than returning
/// `None` (which would silently drop the whole date filter).
fn local_midnight_ms(date: NaiveDate) -> Option<i64> {
    for hour in 0..6 {
        let naive = date.and_hms_opt(hour, 0, 0)?;
        // `.earliest()` also resolves a fall-back day (midnight twice) to the
        // earlier instant, the correct start-of-day.
        if let Some(dt) = Local.from_local_datetime(&naive).earliest() {
            return Some(dt.timestamp_millis());
        }
    }
    None
}

fn date_within(value: Option<i64>, filter: &DateFilter) -> bool {
    let Some(ms) = value else {
        return false; // unknown dates never satisfy a date filter
    };
    if filter.min.is_some_and(|m| ms < m) {
        return false;
    }
    if filter.max.is_some_and(|m| ms > m) {
        return false;
    }
    true
}

/// Parse an `attrib:` value (a run of attribute letters) into a bitmask that the
/// entry must have fully set. Unknown letters are ignored; an empty mask yields
/// `None` so the predicate is dropped rather than matching everything.
fn parse_attrib(value: &str) -> Option<u32> {
    let mut mask = 0u32;
    for c in value.chars() {
        mask |= match c.to_ascii_lowercase() {
            'r' => 0x0000_0001, // readonly
            'h' => 0x0000_0002, // hidden
            's' => 0x0000_0004, // system
            'd' => 0x0000_0010, // directory
            'a' => 0x0000_0020, // archive
            'n' => 0x0000_0080, // normal
            't' => 0x0000_0100, // temporary
            'p' => 0x0000_0200, // sparse file
            'l' => 0x0000_0400, // reparse point
            'c' => 0x0000_0800, // compressed
            'o' => 0x0000_1000, // offline
            'i' => 0x0000_2000, // not content indexed
            'e' => 0x0000_4000, // encrypted
            'v' => 0x0001_0000, // virtual
            _ => continue,
        };
    }
    (mask != 0).then_some(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(query: &str) -> Matcher {
        Matcher::compile(&SearchOptions {
            query: query.into(),
            ..SearchOptions::default()
        })
        .unwrap()
    }

    fn ww_matcher(query: &str) -> Matcher {
        Matcher::compile(&SearchOptions {
            query: query.into(),
            whole_word: true,
            ..SearchOptions::default()
        })
        .unwrap()
    }

    fn view<'a>(
        name: &'a str,
        name_lower: &'a str,
        is_dir: bool,
        size: Option<u64>,
    ) -> EntryView<'a> {
        EntryView {
            name,
            name_lower,
            path: "",
            path_lower: "",
            is_dir,
            size,
            modified_ms: None,
            created_ms: None,
            accessed_ms: None,
            attributes: 0,
        }
    }

    fn hits(m: &Matcher, name: &str, is_dir: bool, size: Option<u64>) -> bool {
        m.eval(&view(name, &name.to_lowercase(), is_dir, size))
    }

    /// Evaluate against an entry that carries a reconstructed full path, so
    /// path-target predicates can be exercised.
    fn path_hits(m: &Matcher, full_path: &str) -> bool {
        let name = full_path.rsplit('\\').next().unwrap_or(full_path);
        let name_lower = name.to_lowercase();
        let path_lower = full_path.to_lowercase();
        m.eval(&EntryView {
            name,
            name_lower: &name_lower,
            path: full_path,
            path_lower: &path_lower,
            is_dir: false,
            size: None,
            modified_ms: None,
            created_ms: None,
            accessed_ms: None,
            attributes: 0,
        })
    }

    fn eval_attr(m: &Matcher, attributes: u32) -> bool {
        let mut v = view("x", "x", false, None);
        v.attributes = attributes;
        m.eval(&v)
    }

    fn eval_modified(m: &Matcher, modified_ms: Option<i64>) -> bool {
        let mut v = view("x", "x", false, None);
        v.modified_ms = modified_ms;
        m.eval(&v)
    }

    fn eval_created(m: &Matcher, created_ms: Option<i64>) -> bool {
        let mut v = view("x", "x", false, None);
        v.created_ms = created_ms;
        m.eval(&v)
    }

    fn eval_accessed(m: &Matcher, accessed_ms: Option<i64>) -> bool {
        let mut v = view("x", "x", false, None);
        v.accessed_ms = accessed_ms;
        m.eval(&v)
    }

    #[test]
    fn dc_and_da_read_their_own_field_not_modified() {
        let ms = 1_700_000_000_000; // 2023-11-14
        // dc: only looks at created_ms — an entry with a matching modified/accessed but no created is a miss.
        assert!(eval_created(&matcher("dc:2023"), Some(ms)));
        assert!(!eval_created(&matcher("dc:2023"), None));
        assert!(!eval_modified(&matcher("dc:2023"), Some(ms))); // modified_ms is irrelevant to dc:
        assert!(!eval_accessed(&matcher("dc:2023"), Some(ms))); // accessed_ms is irrelevant to dc:

        assert!(eval_accessed(&matcher("da:2023"), Some(ms)));
        assert!(!eval_accessed(&matcher("da:2023"), None));
        assert!(!eval_created(&matcher("da:2023"), Some(ms)));
    }

    #[test]
    fn implicit_and() {
        let m = matcher("foo bar");
        assert!(hits(&m, "a-foo-bar.txt", false, None));
        assert!(!hits(&m, "foo.txt", false, None));
    }

    #[test]
    fn or_operator() {
        let m = matcher("foo | bar");
        assert!(hits(&m, "foo.txt", false, None));
        assert!(hits(&m, "bar.txt", false, None));
        assert!(!hits(&m, "baz.txt", false, None));
    }

    #[test]
    fn not_operator() {
        let m = matcher("report !draft");
        assert!(hits(&m, "report-final.txt", false, None));
        assert!(!hits(&m, "report-draft.txt", false, None));
    }

    #[test]
    fn quoted_phrase_is_literal() {
        let m = matcher("\"a | b\"");
        assert!(hits(&m, "x a | b y", false, None));
        assert!(!hits(&m, "a then b", false, None));
    }

    #[test]
    fn functions_combine_with_booleans() {
        let m = matcher("ext:dll | ext:exe");
        assert!(hits(&m, "user32.dll", false, None));
        assert!(hits(&m, "app.exe", false, None));
        assert!(!hits(&m, "notes.txt", false, None));

        let m = matcher("folder: data");
        assert!(hits(&m, "data", true, None));
        assert!(!hits(&m, "data.txt", false, None));

        let m = matcher("size:>1mb");
        assert!(hits(&m, "big.bin", false, Some(2 * 1024 * 1024)));
        assert!(!hits(&m, "small.bin", false, Some(1024)));
    }

    #[test]
    fn empty_matches_all() {
        let m = matcher("");
        assert!(m.matches_all());
        assert!(hits(&m, "anything", false, None));
    }

    #[test]
    fn bare_path_query_lists_folder_contents() {
        // Typing a folder path matches the full path (Everything-style) so the
        // folder's contents — and descendants — show, not zero results.
        let m = matcher(r"C:\Users\Swatto\Downloads\");
        assert!(m.needs_path());
        assert!(path_hits(&m, r"C:\Users\Swatto\Downloads\report.pdf"));
        assert!(path_hits(&m, r"C:\Users\Swatto\Downloads\sub\deep.txt"));
        assert!(!path_hits(&m, r"C:\Users\Swatto\Documents\report.pdf"));
    }

    #[test]
    fn drive_letter_query_targets_that_drive() {
        let m = matcher("c:");
        assert!(m.needs_path());
        assert!(path_hits(&m, r"C:\Windows\notepad.exe"));
    }

    #[test]
    fn relative_path_fragment_targets_the_path() {
        let m = matcher(r"Downloads\report");
        assert!(m.needs_path());
        assert!(path_hits(&m, r"C:\Users\Swatto\Downloads\report.pdf"));
        assert!(!path_hits(&m, r"C:\Users\Swatto\Downloads\notes.txt"));
    }

    #[test]
    fn plain_word_still_matches_name_only() {
        // Regression guard: an ordinary term must not switch to path matching.
        let m = matcher("report");
        assert!(!m.needs_path());
        assert!(hits(&m, "report.pdf", false, None));
    }

    #[test]
    fn whole_word_matches_punctuation_edged_names() {
        // A non-word-edged token matches at any word boundary: the whole-name case
        // (`.gitignore`), the common extension case (`.log` in `system.log` — the
        // v0.15.0 regression this guards), and the glued-suffix case.
        assert!(hits(&ww_matcher(".gitignore"), ".gitignore", false, None));
        assert!(hits(&ww_matcher(".log"), "system.log", false, None));
        assert!(hits(&ww_matcher(".cfg"), "app.cfg", false, None));
        assert!(hits(&ww_matcher("C++"), "C++", false, None));
        assert!(hits(&ww_matcher("+++"), "+++", false, None));
        // `.log` must not match a name that merely contains "log" without the dot.
        assert!(!hits(&ww_matcher(".log"), "catalog", false, None));
        // An ordinary word token still behaves as a whole-word match.
        assert!(hits(&ww_matcher("report"), "the report.txt", false, None));
        assert!(!hits(&ww_matcher("report"), "reporter.txt", false, None));
    }

    #[test]
    fn malformed_filter_value_matches_nothing_not_everything() {
        // A recognised filter with a non-empty but unparseable value returns zero
        // results, never the whole index. (An empty value stays a harmless drop.)
        for q in [
            "size:gb",
            "size:>",
            "size:abc",
            "dm:2024-02-30",
            "attrib:zzz",
        ] {
            let m = matcher(q);
            assert!(!m.matches_all(), "{q} must not collapse to match-all");
            assert!(!hits(&m, "anything.txt", false, Some(4096)), "{q} matched");
        }
        // A bare prefix (mid-typing) is still dropped, not a never-match.
        assert!(matcher("size:").matches_all());
        assert!(matcher("dm:").matches_all());
    }

    #[test]
    fn size_explicit_equals_and_negative() {
        assert!(hits(&matcher("size:=0"), "empty.txt", false, Some(0)));
        assert!(!hits(&matcher("size:=0"), "data.bin", false, Some(8)));
        assert!(hits(
            &matcher("size:=8mb"),
            "f.bin",
            false,
            Some(8 * 1024 * 1024)
        ));
        // size:<0 matches nothing (not 0-byte files).
        let m = matcher("size:<0");
        assert!(!m.matches_all());
        assert!(!hits(&m, "empty.txt", false, Some(0)));
    }

    #[test]
    fn unknown_size_is_not_treated_as_zero() {
        // A file whose size is unknown must satisfy no bounded size filter.
        assert!(!hits(&matcher("size:0"), "x", false, None));
        assert!(!hits(&matcher("size:>0"), "x", false, None));
    }

    #[test]
    fn open_ended_and_reversed_date_ranges() {
        let ms = Some(1_718_452_800_000); // 2024-06-15
                                          // `..B` => up to B; `A..` => from A.
        assert!(eval_modified(&matcher("dm:..2024-12-31"), ms));
        assert!(!eval_modified(&matcher("dm:..2024-01-01"), ms));
        assert!(eval_modified(&matcher("dm:2024-01-01.."), ms));
        assert!(!eval_modified(&matcher("dm:2025-01-01.."), ms));
        assert!(!matcher("dm:..2024-12-31").matches_all());
        // Reversed endpoints behave like the forward range.
        assert!(eval_modified(&matcher("dm:2024-12-31..2024-01-01"), ms));
    }

    #[test]
    fn ext_tolerates_leading_dot() {
        assert!(hits(&matcher("ext:.dll"), "user32.dll", false, None));
        assert!(hits(&matcher("ext:.dll,.exe"), "app.exe", false, None));
    }

    #[test]
    fn size_ranges() {
        let m = matcher("size:1mb..5mb");
        assert!(hits(&m, "f", false, Some(2 * 1024 * 1024)));
        assert!(!hits(&m, "f", false, Some(512 * 1024)));
        assert!(!hits(&m, "f", false, Some(6 * 1024 * 1024)));
        // Reversed endpoints behave like the forward range.
        assert!(hits(
            &matcher("size:5mb..1mb"),
            "f",
            false,
            Some(2 * 1024 * 1024)
        ));
        // One-sided ranges.
        assert!(hits(&matcher("size:..1kb"), "f", false, Some(512)));
        assert!(!hits(&matcher("size:..1kb"), "f", false, Some(4096)));
        assert!(hits(
            &matcher("size:1mb.."),
            "f",
            false,
            Some(2 * 1024 * 1024)
        ));
    }

    #[test]
    fn case_folding_consistent_plain_and_glob() {
        // A query gives the same hits whether or not it carries a wildcard, even
        // for non-ASCII where to_lowercase and the regex crate's folding differ.
        // 'Σ' folds (to_lowercase) to 'σ'; query 'ς' must miss it both ways.
        assert_eq!(
            hits(&matcher("ς"), "Σtest", false, None),
            hits(&matcher("*ς*"), "Σtest", false, None),
        );
        // A normal ASCII glob is still case-insensitive by default.
        assert!(hits(&matcher("*.PDF"), "report.pdf", false, None));
    }

    #[test]
    fn path_fragment_glob_matches_interior() {
        // A path fragment with a wildcard matches an interior path segment, the
        // same as the fragment without a wildcard.
        let m = matcher(r"Downloads\*.pdf");
        assert!(m.needs_path());
        assert!(path_hits(&m, r"C:\Users\Swatto\Downloads\report.pdf"));
        assert!(!path_hits(&m, r"C:\Users\Swatto\Downloads\notes.txt"));
        // A drive-rooted wildcard still anchors at the drive, but a single `*`
        // stays within one path segment — it does not cross `\`.
        assert!(path_hits(&matcher(r"C:\Users\*.pdf"), r"C:\Users\b.pdf"));
        assert!(!path_hits(&matcher(r"C:\Users\*.pdf"), r"C:\Users\a\b.pdf"));
        assert!(!path_hits(&matcher(r"C:\Users\*.pdf"), r"D:\Other\b.pdf"));
    }

    #[test]
    fn double_star_crosses_separators_single_star_does_not() {
        let single = matcher(r"C:\Users\*.pdf");
        assert!(!path_hits(&single, r"C:\Users\a\b.pdf"));
        let double = matcher(r"C:\Users\**.pdf");
        assert!(path_hits(&double, r"C:\Users\a\b.pdf"));
        assert!(path_hits(&double, r"C:\Users\b.pdf"));
    }

    #[test]
    fn function_prefixes_are_case_insensitive() {
        assert!(hits(
            &matcher("SIZE:>1mb"),
            "big.bin",
            false,
            Some(2 * 1024 * 1024)
        ));
        assert!(!hits(&matcher("Size:>1mb"), "small.bin", false, Some(1024)));
        assert!(hits(&matcher("EXT:dll"), "user32.dll", false, None));
        assert!(eval_modified(&matcher("DM:2024"), Some(1_718_452_800_000)));
        // A drive-letter path (`D:`) must NOT be mistaken for a function prefix.
        assert!(matcher("D:").needs_path());
    }

    #[test]
    fn ext_filter_folds_non_ascii() {
        // `ext:` must Unicode-lowercase to match the index's `name_lower`.
        assert!(hits(&matcher("ext:CAFÉ"), "x.CAFÉ", false, None));
        assert!(hits(&matcher("ext:café"), "x.CAFÉ", false, None));
    }

    #[test]
    fn attrib_requires_all_listed_bits() {
        let m = matcher("attrib:h");
        assert!(eval_attr(&m, 0x2)); // hidden
        assert!(eval_attr(&m, 0x2 | 0x20)); // hidden + archive
        assert!(!eval_attr(&m, 0x20)); // archive only

        let m = matcher("attrib:hs");
        assert!(eval_attr(&m, 0x2 | 0x4)); // hidden + system
        assert!(!eval_attr(&m, 0x2)); // hidden but not system
    }

    #[test]
    fn date_filters_use_inclusive_bounds() {
        // 2024-06-15T12:00:00Z — months from any year boundary, so timezone
        // offset can't move it across the asserted edges.
        let ms = Some(1_718_452_800_000);

        assert!(eval_modified(&matcher("dm:>=2024-01-01"), ms));
        assert!(!eval_modified(&matcher("dm:<2024-01-01"), ms));
        assert!(eval_modified(&matcher("dm:2024-01-01..2024-12-31"), ms));
        assert!(eval_modified(&matcher("dm:2024"), ms));
        assert!(!eval_modified(&matcher("dm:2023"), ms));
    }

    #[test]
    fn unknown_date_never_matches() {
        assert!(!eval_modified(&matcher("dm:>=2000"), None));
    }

    #[test]
    fn out_of_range_year_matches_nothing_not_panicking() {
        // Years past chrono's range (and i32::MAX, which would overflow `year+1`)
        // must not panic. The value is non-empty but unparseable, so the query
        // matches nothing rather than silently widening to the whole index.
        for q in [
            "dm:2147483647",
            "dm:>=999999999",
            "dm:2147483647..2147483647",
        ] {
            let m = matcher(q);
            assert!(!m.matches_all(), "{q} must not collapse to match-all");
            assert!(
                !eval_modified(&m, Some(1_718_452_800_000)),
                "{q} must match nothing"
            );
        }
    }

    #[test]
    fn grouping_distributes_or_under_and() {
        // a AND (b OR c) — without the group this would parse as (a AND b) OR c.
        let m = matcher("a (b | c)");
        assert!(hits(&m, "a-b.txt", false, None));
        assert!(hits(&m, "a-c.txt", false, None));
        assert!(!hits(&m, "b-c.txt", false, None)); // missing `a`
        assert!(!hits(&m, "a-only.txt", false, None)); // neither b nor c
    }

    #[test]
    fn ungrouped_precedence_is_and_over_or() {
        // Sanity check the contrast: `a b | c` is (a AND b) OR c.
        let m = matcher("a b | c");
        assert!(hits(&m, "a-b.txt", false, None)); // a AND b
        assert!(hits(&m, "c-only.txt", false, None)); // c alone
        assert!(!hits(&m, "a-only.txt", false, None)); // a without b, and no c
    }

    #[test]
    fn nested_groups() {
        // (report | memo) (2023 | 2024) !draft
        let m = matcher("(report | memo) (2023 | 2024) !draft");
        assert!(hits(&m, "report-2023-final.txt", false, None));
        assert!(hits(&m, "memo-2024.txt", false, None));
        assert!(!hits(&m, "report-2022.txt", false, None)); // wrong year
        assert!(!hits(&m, "report-2023-draft.txt", false, None)); // excluded
        assert!(!hits(&m, "letter-2024.txt", false, None)); // neither report nor memo
    }

    #[test]
    fn negated_group_excludes_whole_subexpression() {
        // log !(old | backup) — logs that are neither old nor backup.
        let m = matcher("log !(old | backup)");
        assert!(hits(&m, "system.log", false, None));
        assert!(!hits(&m, "old.log", false, None));
        assert!(!hits(&m, "backup.log", false, None));
        assert!(!hits(&m, "notes.txt", false, None)); // not a log at all
    }

    #[test]
    fn functions_inside_groups() {
        // (ext:dll | ext:exe) size:>1mb — large binaries only.
        let m = matcher("(ext:dll | ext:exe) size:>1mb");
        assert!(hits(&m, "big.dll", false, Some(2 * 1024 * 1024)));
        assert!(hits(&m, "app.exe", false, Some(4 * 1024 * 1024)));
        assert!(!hits(&m, "small.dll", false, Some(1024))); // too small
        assert!(!hits(&m, "big.txt", false, Some(2 * 1024 * 1024))); // wrong ext
    }

    #[test]
    fn unbalanced_parens_are_lenient() {
        // Stray/unclosed parentheses must degrade gracefully, never error or
        // swallow the whole query.
        assert!(hits(&matcher("(foo"), "foo.txt", false, None)); // unclosed open
        assert!(hits(&matcher("foo)"), "foo.txt", false, None)); // stray close
        assert!(hits(&matcher(")foo"), "foo.txt", false, None)); // leading stray
        assert!(hits(&matcher("foo (bar"), "foo-bar.txt", false, None));
        // An empty group is no constraint.
        assert!(hits(&matcher("foo ()"), "foo.txt", false, None));
        assert!(matcher("()").matches_all());
    }

    #[test]
    fn deeply_nested_parens_do_not_overflow() {
        // A pathological run of '(' must be bounded, not recurse without limit
        // (with panic = "abort" a stack overflow would abort the process). The
        // inner term still matches; the surplus parens are flattened away.
        let depth = (MAX_GROUP_DEPTH as usize) * 4;
        let q = format!("{}foo{}", "(".repeat(depth), ")".repeat(depth));
        let m = matcher(&q);
        assert!(hits(&m, "foo.txt", false, None));
        assert!(!hits(&m, "bar.txt", false, None));
    }

    #[test]
    fn long_not_run_does_not_overflow() {
        // A run of `!` must fold to O(1) parser depth, not recurse per `!`
        // (uncapped `!` recursion would overflow the stack — fatal under
        // panic = "abort"). Far past any stack the recursive form survived.
        let bangs = "!".repeat(500_000);
        // Even count cancels: `!!…!!foo` == `foo`.
        let even = matcher(&format!("{bangs}foo"));
        assert!(hits(&even, "foo.txt", false, None));
        assert!(!hits(&even, "bar.txt", false, None));
        // Odd count negates: `!!…!foo` == `!foo`.
        let odd = matcher(&format!("{}foo", "!".repeat(500_001)));
        assert!(!hits(&odd, "foo.txt", false, None));
        assert!(hits(&odd, "bar.txt", false, None));
    }

    #[test]
    fn double_negation_cancels() {
        let m = matcher("report !!draft");
        // `!!draft` == `draft`, so both terms are required (implicit AND).
        assert!(hits(&m, "report-draft.txt", false, None));
        assert!(!hits(&m, "report-final.txt", false, None));
    }

    #[test]
    fn quoted_parens_are_literal() {
        // A quoted parenthesis is matched literally, not treated as grouping.
        let m = matcher("\"(x86)\"");
        assert!(hits(&m, "Program Files (x86)", true, None));
        assert!(!hits(&m, "Program Files", true, None));
    }

    #[test]
    fn date_keywords_parse() {
        // Relative keywords depend on "now"; assert only that they compile to a
        // usable predicate (not dropped) rather than a specific instant.
        for kw in [
            "today",
            "yesterday",
            "thisweek",
            "lastweek",
            "thismonth",
            "lastmonth",
            "thisyear",
            "lastyear",
        ] {
            let m = matcher(&format!("dm:{kw}"));
            assert!(!m.matches_all(), "dm:{kw} should be a real predicate");
        }
    }

    fn opts_matcher(opts: SearchOptions) -> Matcher {
        Matcher::compile(&opts).unwrap()
    }

    #[test]
    fn regex_mode_matches_and_is_case_toggle_aware() {
        let insensitive = opts_matcher(SearchOptions { query: r"^report.*\.pdf$".into(), regex: true, ..Default::default() });
        assert!(hits(&insensitive, "Report_2024.pdf", false, None));
        let case_sensitive = opts_matcher(SearchOptions { query: r"^report.*\.pdf$".into(), regex: true, match_case: true, ..Default::default() });
        assert!(!hits(&case_sensitive, "Report_2024.pdf", false, None));
        assert!(hits(&case_sensitive, "report_2024.pdf", false, None));
    }

    #[test]
    fn invalid_regex_is_a_compile_error_not_a_silent_match() {
        assert!(Matcher::compile(&SearchOptions { query: "(unclosed".into(), regex: true, ..Default::default() }).is_err());
    }

    #[test]
    fn match_case_toggle_affects_plain_and_wildcard_text() {
        let insensitive = matcher("report");
        assert!(hits(&insensitive, "REPORT.pdf", false, None));
        let sensitive = opts_matcher(SearchOptions { query: "report".into(), match_case: true, ..Default::default() });
        assert!(!hits(&sensitive, "REPORT.pdf", false, None));
        assert!(hits(&sensitive, "report.pdf", false, None));
        let glob_sensitive = opts_matcher(SearchOptions { query: "*.PDF".into(), match_case: true, ..Default::default() });
        assert!(hits(&glob_sensitive, "a.PDF", false, None));
        assert!(!hits(&glob_sensitive, "a.pdf", false, None));
    }

    #[test]
    fn diacritics_toggle_ignores_accents() {
        let plain = matcher("resume");
        assert!(!hits(&plain, "résumé.pdf", false, None));
        let insensitive = opts_matcher(SearchOptions { query: "resume".into(), diacritics: true, ..Default::default() });
        assert!(hits(&insensitive, "résumé.pdf", false, None));
        // The needle itself may carry accents too.
        let accented_query = opts_matcher(SearchOptions { query: "résumé".into(), diacritics: true, ..Default::default() });
        assert!(hits(&accented_query, "resume.pdf", false, None));
    }

    #[test]
    fn child_lists_immediate_contents_only() {
        let m = matcher(r"child:C:\Users\bob\Downloads");
        assert!(m.needs_path());
        assert!(path_hits(&m, r"C:\Users\bob\Downloads\report.pdf"));
        assert!(!path_hits(&m, r"C:\Users\bob\Downloads\sub\report.pdf")); // one level too deep
        assert!(!path_hits(&m, r"C:\Users\bob\Documents\report.pdf")); // wrong folder
    }

    #[test]
    fn quoted_relation_path_may_contain_spaces() {
        let m = matcher(r#"child:"C:\Program Files""#);
        assert!(path_hits(&m, r"C:\Program Files\app.exe"));
        assert!(!path_hits(&m, r"C:\Program Files\App\x.dll"));
        assert!(!path_hits(&m, r"C:\Windows\app.exe"));
    }


    #[test]
    fn child_of_a_drive_root_lists_the_top_level() {
        let m = matcher(r#"child:"C:""#);
        assert!(path_hits(&m, r"C:\Windows"));
        assert!(!path_hits(&m, r"C:\Windows\System32"));
        let bare = matcher("child:C:");
        assert!(path_hits(&bare, r"C:\Windows"));
    }
    #[test]
    fn home_query_selects_exactly_the_listed_folders() {
        let m = matcher(r#"parent:"C:\Users\bob\My Docs\x" | parent:"C:\Work (2)\x""#);
        assert!(path_hits(&m, r"C:\Users\bob\My Docs"));
        assert!(path_hits(&m, r"C:\Work (2)"));
        assert!(!path_hits(&m, r"C:\Users\bob\My Docs\sub"));
        assert!(!path_hits(&m, r"C:\Users\bob"));
    }

    #[test]
    fn parent_finds_the_single_containing_folder() {
        let m = matcher(r"parent:C:\Users\bob\Downloads\report.pdf");
        assert!(path_hits(&m, r"C:\Users\bob\Downloads"));
        assert!(!path_hits(&m, r"C:\Users\bob\Downloads\report.pdf"));
        assert!(!path_hits(&m, r"C:\Users\bob"));
    }

    #[test]
    fn sibling_shares_the_parent_but_excludes_self() {
        let m = matcher(r"sibling:C:\Users\bob\Downloads\report.pdf");
        assert!(path_hits(&m, r"C:\Users\bob\Downloads\notes.txt"));
        assert!(!path_hits(&m, r"C:\Users\bob\Downloads\report.pdf")); // itself
        assert!(!path_hits(&m, r"C:\Users\bob\Documents\notes.txt")); // different folder
    }

    #[test]
    fn descendant_and_ancestor_are_opposite_relations_at_any_depth() {
        let desc = matcher(r"descendant:C:\Users\bob");
        assert!(path_hits(&desc, r"C:\Users\bob\Downloads\a\b\report.pdf"));
        assert!(!path_hits(&desc, r"C:\Users\bob")); // not itself
        assert!(!path_hits(&desc, r"C:\Users\alice\report.pdf"));

        let anc = matcher(r"ancestor:C:\Users\bob\Downloads\a\b\report.pdf");
        assert!(path_hits(&anc, r"C:\Users\bob"));
        assert!(path_hits(&anc, r"C:\Users\bob\Downloads\a"));
        assert!(!path_hits(&anc, r"C:\Users\bob\Downloads\a\b\report.pdf")); // not itself
        assert!(!path_hits(&anc, r"C:\Users\alice"));
    }

    fn dir_hits(m: &Matcher, full_path: &str, is_dir: bool) -> bool {
        let name = full_path.rsplit('\\').next().unwrap_or(full_path);
        let name_lower = name.to_lowercase();
        let path_lower = full_path.to_lowercase();
        m.eval(&EntryView {
            name,
            name_lower: &name_lower,
            path: full_path,
            path_lower: &path_lower,
            is_dir,
            size: None,
            modified_ms: None,
            created_ms: None,
            accessed_ms: None,
            attributes: 0,
        })
    }

    #[test]
    fn empty_and_exists_are_real_filesystem_checks() {
        let base = std::env::temp_dir().join(format!("rn-structural-{}", std::process::id()));
        let full_dir = base.join("full");
        let empty_dir = base.join("empty");
        std::fs::create_dir_all(&full_dir).unwrap();
        std::fs::create_dir_all(&empty_dir).unwrap();
        let file = full_dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();

        let empty_m = matcher("empty:");
        assert!(dir_hits(&empty_m, empty_dir.to_str().unwrap(), true));
        assert!(!dir_hits(&empty_m, full_dir.to_str().unwrap(), true));
        assert!(!dir_hits(&empty_m, file.to_str().unwrap(), false)); // a file, not a directory

        let exists_m = matcher("exists:");
        assert!(dir_hits(&exists_m, file.to_str().unwrap(), false));
        assert!(!dir_hits(&exists_m, base.join("nope.txt").to_str().unwrap(), false));

        let _ = std::fs::remove_dir_all(&base);
    }
}
