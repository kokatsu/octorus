//! Tree-sitter tags based symbol extraction and repository-wide symbol index.
//!
//! This is the code-intelligence layer that turns octorus from a diff reader
//! into a repository viewer. Unlike [`crate::symbol`] — which greps for
//! definition keyword prefixes — symbols here come from the concrete syntax
//! tree, so `fn parse` and the word `parse` inside a comment are never
//! confused.
//!
//! The queries are the same `tags.scm` files GitHub uses for its own code
//! navigation, so there is no language server to install, nothing to index
//! ahead of time, and no daemon to keep alive: the grammars are already
//! compiled into the binary.
//!
//! Two entry points:
//!
//! - [`extract_symbols`] — the outline of a single file, in source order.
//! - [`SymbolIndex`] — every symbol in the repository, queryable by exact name
//!   ([`SymbolIndex::definitions`]) or by fuzzy match
//!   ([`SymbolIndex::search`]).

use std::collections::HashMap;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use smallvec::SmallVec;
use tree_sitter::{QueryCursor, StreamingIterator};

use crate::language::SupportedLanguage;
use crate::syntax::ParserPool;

/// Files larger than this are skipped when indexing.
///
/// Generated bundles and vendored blobs are the usual inhabitants of this
/// bucket, and parsing them costs far more than the symbols are worth.
pub const MAX_INDEXED_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Maximum number of symbols retained per file.
///
/// Guards against pathological generated sources producing a multi-megabyte
/// outline.
pub const MAX_SYMBOLS_PER_FILE: usize = 10_000;

/// Number of metadata pre-filter entries processed between cancellation polls.
const PREFILTER_CANCEL_POLL_INTERVAL: usize = 128;

/// Repository-relative path that makes an indexing worker panic.
///
/// The join-error arm of [`SymbolIndex::build_cancellable`] is otherwise
/// unreachable from a test, and without a guard it can be replaced by a
/// `continue` that silently returns a short index. Test-only so nothing in the
/// production build can reach it.
#[cfg(test)]
const WORKER_PANIC_PROBE_PATH: &str = "src/__octorus_worker_panic_probe.rs";

/// A cooperative cancellation signal polled by [`SymbolIndex::build_cancellable`].
///
/// Abstracted over [`tokio_util::sync::CancellationToken`] rather than taking
/// one directly so the build's polling *granularity* is directly testable: a
/// test signal that fires after a fixed number of polls pins down exactly how
/// many files a cancelled build touches, with no timing assumption and no
/// flaky wall-clock bound.
pub trait CancelSignal: Sync {
    fn is_cancelled(&self) -> bool;
}

impl CancelSignal for tokio_util::sync::CancellationToken {
    fn is_cancelled(&self) -> bool {
        tokio_util::sync::CancellationToken::is_cancelled(self)
    }
}

/// The kind of a named entity, derived from the `@definition.*` capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Interface,
    Module,
    Macro,
    Constant,
    Type,
    Field,
    Property,
    /// Markdown heading — the outline of a prose document.
    Heading,
}

impl SymbolKind {
    /// Map a tags capture name (`definition.function`) to a kind.
    ///
    /// Unknown capture names return `None` and are skipped, so a grammar that
    /// grows a new `@definition.*` capture degrades to "not shown" rather than
    /// to a misleading icon.
    fn from_capture(capture: &str) -> Option<Self> {
        match capture.strip_prefix("definition.")? {
            "function" => Some(Self::Function),
            "method" => Some(Self::Method),
            "class" | "struct" | "enum" | "union" => Some(Self::Class),
            "interface" | "trait" | "protocol" => Some(Self::Interface),
            "module" | "namespace" | "package" => Some(Self::Module),
            "macro" => Some(Self::Macro),
            "constant" => Some(Self::Constant),
            "type" => Some(Self::Type),
            "field" => Some(Self::Field),
            "property" => Some(Self::Property),
            "heading" => Some(Self::Heading),
            _ => None,
        }
    }

    /// Single-character glyph used in outline and search rows.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Function => "ƒ",
            Self::Method => "m",
            Self::Class => "C",
            Self::Interface => "I",
            Self::Module => "M",
            Self::Macro => "!",
            Self::Constant => "c",
            Self::Type => "T",
            Self::Field => "f",
            Self::Property => "p",
            Self::Heading => "#",
        }
    }
}

/// A named entity in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// 1-based line of the symbol's name.
    pub line: usize,
    /// 0-based column (in characters) of the symbol's name.
    pub column: usize,
    /// Nesting depth of the enclosing definitions — 0 for top level.
    pub depth: usize,
}

/// A symbol together with the file it lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolRef<'a> {
    pub path: &'a str,
    pub symbol: &'a Symbol,
}

impl SymbolRef<'_> {
    /// Single source of truth for a symbol-search result row: `ƒ name  path:line`.
    ///
    /// Callers render this label directly instead of rebuilding the row format.
    pub fn search_label(&self) -> String {
        format!(
            "{} {}  {}:{}",
            self.symbol.kind.glyph(),
            self.symbol.name,
            self.path,
            self.symbol.line
        )
    }
}

/// Whether symbol extraction is possible for the given filename.
pub fn supports_symbols(filename: &str) -> bool {
    language_for_file(filename).is_some()
}

/// Resolve a filename to a language that has a tags query.
fn language_for_file(filename: &str) -> Option<SupportedLanguage> {
    let ext = Path::new(filename).extension()?.to_str()?;
    let lang = SupportedLanguage::from_extension(ext)?;
    lang.tags_query().map(|_| lang)
}

/// Intermediate match before nesting depth is known.
struct RawTag {
    name: String,
    kind: SymbolKind,
    line: usize,
    column: usize,
    /// Byte offset of the name node — the identity of the tagged entity.
    name_byte: usize,
    /// Byte range of the whole definition node, used to derive nesting.
    start_byte: usize,
    end_byte: usize,
}

/// Preference order when several patterns tag the same name node.
///
/// Rust's `tags.scm`, for example, matches an `impl` body's `fn` as both a
/// method and a function. The more specific label wins so the outline reads
/// `m new` rather than listing `new` twice.
fn kind_specificity(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Method => 0,
        SymbolKind::Property => 1,
        SymbolKind::Field => 2,
        SymbolKind::Constant => 3,
        SymbolKind::Macro => 4,
        SymbolKind::Function => 5,
        SymbolKind::Interface => 6,
        SymbolKind::Class => 7,
        SymbolKind::Type => 8,
        SymbolKind::Module => 9,
        SymbolKind::Heading => 10,
    }
}

/// Extract the symbols of a single file, in source order.
///
/// Returns an empty vector when the language is unsupported, the file fails to
/// parse, or it simply contains no named entities. Callers render an empty
/// outline rather than an error — an empty file is a use case, not a failure.
pub fn extract_symbols(source: &str, filename: &str, pool: &mut ParserPool) -> Vec<Symbol> {
    let Some(lang) = language_for_file(filename) else {
        return Vec::new();
    };

    let tree = {
        let Some(parser) = pool.get_or_create(lang.default_extension()) else {
            return Vec::new();
        };
        match parser.parse(source, None) {
            Some(tree) => tree,
            None => return Vec::new(),
        }
    };

    let Some(query) = pool.get_or_create_tags_query(lang) else {
        return Vec::new();
    };

    let capture_names = query.capture_names();
    let bytes = source.as_bytes();
    let mut raw: Vec<RawTag> = Vec::new();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), bytes);
    while let Some(m) = matches.next() {
        let mut name_node = None;
        let mut definition = None;

        for capture in m.captures {
            let capture_name = capture_names[capture.index as usize];
            if capture_name == "name" {
                name_node.get_or_insert(capture.node);
            } else if let Some(kind) = SymbolKind::from_capture(capture_name) {
                definition.get_or_insert((kind, capture.node));
            }
        }

        let (Some(name_node), Some((kind, def_node))) = (name_node, definition) else {
            continue;
        };

        let Ok(name) = name_node.utf8_text(bytes) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }

        let position = name_node.start_position();
        raw.push(RawTag {
            name: name.to_string(),
            kind,
            line: position.row + 1,
            column: char_column(source, name_node.start_byte(), position.column),
            name_byte: name_node.start_byte(),
            start_byte: def_node.start_byte(),
            end_byte: def_node.end_byte(),
        });

        if raw.len() >= MAX_SYMBOLS_PER_FILE {
            break;
        }
    }

    let mut raw = collapse_duplicate_tags(raw);

    // Source order, outer definitions before the ones they contain.
    raw.sort_by(|a, b| {
        a.start_byte
            .cmp(&b.start_byte)
            .then(b.end_byte.cmp(&a.end_byte))
            .then(a.line.cmp(&b.line))
            .then(a.name.cmp(&b.name))
    });

    let mut symbols: Vec<Symbol> = Vec::with_capacity(raw.len());
    // Stack of enclosing definition end offsets; its height is the depth.
    let mut enclosing: Vec<usize> = Vec::new();

    for tag in raw {
        while enclosing.last().is_some_and(|end| *end <= tag.start_byte) {
            enclosing.pop();
        }
        let depth = enclosing.len();
        enclosing.push(tag.end_byte);

        // Grammars such as Haskell emit one match per defining equation, and
        // some tags queries match a declaration through two patterns. Collapse
        // only *adjacent* repeats so genuinely distinct same-named symbols
        // (a method repeated across impl blocks) are preserved.
        if let Some(previous) = symbols.last() {
            if previous.name == tag.name && previous.kind == tag.kind && previous.depth == depth {
                continue;
            }
        }

        symbols.push(Symbol {
            name: tag.name,
            kind: tag.kind,
            line: tag.line,
            column: tag.column,
            depth,
        });
    }

    symbols
}

/// Keep one tag per tagged name node.
///
/// Several patterns in the same `tags.scm` routinely match one entity — the
/// winner is the most specific kind, and among equal kinds the tightest
/// definition node, so nesting is computed from the innermost enclosing
/// construct.
fn collapse_duplicate_tags(raw: Vec<RawTag>) -> Vec<RawTag> {
    let mut best: HashMap<usize, RawTag> = HashMap::with_capacity(raw.len());

    for tag in raw {
        match best.entry(tag.name_byte) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(tag);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let current = slot.get();
                let span = tag.end_byte.saturating_sub(tag.start_byte);
                let current_span = current.end_byte.saturating_sub(current.start_byte);
                let better = (kind_specificity(tag.kind), span)
                    < (kind_specificity(current.kind), current_span);
                if better {
                    slot.insert(tag);
                }
            }
        }
    }

    best.into_values().collect()
}

/// Convert a tree-sitter byte column to a character column.
///
/// tree-sitter reports columns in bytes; every column consumer in octorus
/// counts characters, so multi-byte identifiers (CJK, accented Latin) would
/// otherwise point past the symbol.
fn char_column(source: &str, start_byte: usize, byte_column: usize) -> usize {
    let line_start = start_byte.saturating_sub(byte_column);
    if line_start >= source.len() || !source.is_char_boundary(line_start) {
        return byte_column;
    }
    let end = start_byte.min(source.len());
    if !source.is_char_boundary(end) {
        return byte_column;
    }
    source[line_start..end].chars().count()
}

/// Symbols of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSymbols {
    pub path: String,
    pub symbols: Vec<Symbol>,
}

/// Outcome of a cancellable repository-wide index build.
///
/// Three distinct outcomes rather than `Option<SymbolIndex>` plus a flag,
/// because the caller renders a different thing for each: the finished index,
/// nothing at all (a newer build superseded this one), or an error banner.
#[derive(Debug)]
pub enum IndexBuild {
    /// The walk visited every indexable path.
    Completed(SymbolIndex),
    /// The signal fired mid-walk; `scanned_files` files had been walked when it did.
    Cancelled { scanned_files: usize },
    /// The build could not run at all — e.g. the repository root vanished
    /// (a worktree removed mid-session), or an indexing worker panicked and the
    /// index would otherwise silently under-report.
    Failed { message: String },
}

/// A search needle, lower-cased by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LoweredNeedle(String);

impl LoweredNeedle {
    fn new(query: &str) -> Self {
        Self(query.to_lowercase())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
struct CachedSearch {
    needle: LoweredNeedle,
    limit: usize,
    hits: Vec<(u32, u32)>,
}

/// A scored search hit ordered by [`compare_search_candidates`].
///
/// The index tie-breakers reproduce the insertion order of the original stable
/// full sort when all user-visible keys are equal.
#[derive(Debug)]
struct SearchCandidate<'a> {
    score: i64,
    name_len: usize,
    path: &'a str,
    line: usize,
    file_index: u32,
    symbol_index: u32,
}

fn compare_search_candidates(
    a: &SearchCandidate<'_>,
    b: &SearchCandidate<'_>,
) -> std::cmp::Ordering {
    b.score
        .cmp(&a.score)
        .then(a.name_len.cmp(&b.name_len))
        .then(a.path.cmp(b.path))
        .then(a.line.cmp(&b.line))
        .then(a.file_index.cmp(&b.file_index))
        .then(a.symbol_index.cmp(&b.symbol_index))
}

/// A repository-wide symbol index.
///
/// Built once per browse session from a list of repository-relative paths and
/// queried synchronously afterwards.
#[derive(Debug, Default)]
pub struct SymbolIndex {
    files: Vec<FileSymbols>,
    /// Lower-cased names parallel to `files`, with `None` when the original is
    /// already lower-case.
    lowered_names: Vec<Vec<Option<Box<str>>>>,
    /// Lower-cased name -> (file index, symbol index) pairs.
    by_name: HashMap<String, SmallVec<[(u32, u32); 2]>>,
    /// Number of files that were walked, including ones with no symbols.
    scanned_files: usize,
    /// Test-only cost probe counting comparator invocations for one search.
    ///
    /// Counts the ordering *work*, not a number recorded next to it: a probe
    /// that reports the post-truncate candidate count reads the same value
    /// whether the top-N partition or an ordinary full sort produced it, so it
    /// cannot tell the two apart. Comparison counts can.
    #[cfg(test)]
    search_comparisons: AtomicUsize,
    /// Last search, kept so the render loop can redraw the overlay without
    /// rescoring the repository: the overlay re-queries on every frame and the
    /// query only changes on a keystroke.
    last_search: Mutex<Option<CachedSearch>>,
}

impl SymbolIndex {
    /// Build an index from already-extracted per-file symbols.
    pub fn from_files(files: Vec<FileSymbols>) -> Self {
        let mut by_name: HashMap<String, SmallVec<[(u32, u32); 2]>> = HashMap::new();
        let mut lowered_names = Vec::with_capacity(files.len());
        let scanned_files = files.len();

        for (file_index, file) in files.iter().enumerate() {
            let mut file_lowered_names = Vec::with_capacity(file.symbols.len());
            for (symbol_index, symbol) in file.symbols.iter().enumerate() {
                let lowered = symbol.name.to_lowercase();
                let cached_lowered =
                    (lowered != symbol.name).then(|| lowered.clone().into_boxed_str());
                by_name
                    .entry(lowered)
                    .or_default()
                    .push((file_index as u32, symbol_index as u32));
                file_lowered_names.push(cached_lowered);
            }
            lowered_names.push(file_lowered_names);
        }

        Self {
            files,
            lowered_names,
            by_name,
            scanned_files,
            ..Self::default()
        }
    }

    /// Build an index by reading and parsing every given repository-relative path.
    ///
    /// Blocking and CPU-bound — call it from `spawn_blocking`, never from the
    /// render loop. Unreadable, oversized and unsupported individual files are
    /// skipped silently; a repository is allowed to contain files octorus
    /// cannot parse.
    pub fn build_cancellable(
        repo_root: &Path,
        paths: &[String],
        cancel: &dyn CancelSignal,
    ) -> IndexBuild {
        let root_metadata = match std::fs::metadata(repo_root) {
            Ok(metadata) => metadata,
            Err(error) => {
                return IndexBuild::Failed {
                    message: format!(
                        "cannot build symbol index: repository root '{}' is unavailable: {error}",
                        repo_root.display()
                    ),
                };
            }
        };
        if !root_metadata.is_dir() {
            return IndexBuild::Failed {
                message: format!(
                    "cannot build symbol index: repository root '{}' is not a directory",
                    repo_root.display()
                ),
            };
        }

        if cancel.is_cancelled() {
            return IndexBuild::Cancelled { scanned_files: 0 };
        }

        let mut indexable = Vec::new();
        for (position, path) in paths.iter().enumerate() {
            if position != 0
                && position % PREFILTER_CANCEL_POLL_INTERVAL == 0
                && cancel.is_cancelled()
            {
                return IndexBuild::Cancelled { scanned_files: 0 };
            }
            if !supports_symbols(path) {
                continue;
            }
            let indexable_file = std::fs::metadata(repo_root.join(path.as_str()))
                .map(|meta| meta.is_file() && meta.len() <= MAX_INDEXED_FILE_BYTES)
                .unwrap_or(false);
            if indexable_file {
                indexable.push(path);
            }
        }

        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 8)
            .min(indexable.len().max(1));

        let chunk_size = indexable.len().div_ceil(workers).max(1);
        let outcomes: Vec<std::thread::Result<ChunkOutcome>> = std::thread::scope(|scope| {
            let handles: Vec<_> = indexable
                .chunks(chunk_size)
                .map(|chunk| scope.spawn(move || index_chunk(repo_root, chunk, cancel)))
                .collect();

            handles.into_iter().map(|handle| handle.join()).collect()
        });

        let mut files = Vec::new();
        let mut scanned_files = 0;
        let mut stopped_early = false;
        for outcome in outcomes {
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(_) => {
                    return IndexBuild::Failed {
                        message: format!(
                            "symbol indexing worker panicked for repository '{}'; retry the build",
                            repo_root.display()
                        ),
                    };
                }
            };
            files.extend(outcome.files);
            scanned_files += outcome.scanned;
            stopped_early |= outcome.stopped_early;
        }

        if stopped_early {
            return IndexBuild::Cancelled { scanned_files };
        }

        // Chunks finish out of order; a stable path order keeps search results
        // and snapshots deterministic.
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut index = Self::from_files(files);
        index.scanned_files = scanned_files;
        IndexBuild::Completed(index)
    }

    /// Test-only probe for distinguishing files with symbols from scanned files.
    #[cfg(test)]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Test-only probe for build and cancellation accounting assertions.
    #[cfg(test)]
    pub fn scanned_file_count(&self) -> usize {
        self.scanned_files
    }

    /// Total number of indexed symbols.
    pub fn symbol_count(&self) -> usize {
        self.files.iter().map(|file| file.symbols.len()).sum()
    }

    /// Test-only probe for empty-index behavior; production reads `symbol_count`.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.symbol_count() == 0
    }

    /// Test-only cost probe: comparator invocations made by the last [`Self::search`].
    #[cfg(test)]
    pub fn search_comparisons(&self) -> usize {
        self.search_comparisons.load(Ordering::Relaxed)
    }

    /// Symbols of a single indexed file, if it was indexed.
    pub fn file_symbols(&self, path: &str) -> Option<&[Symbol]> {
        self.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.symbols.as_slice())
    }

    /// All definitions with exactly this name, case-insensitively.
    ///
    /// Ordered so the best jump target comes first: callable definitions before
    /// containers before fields, shallower before deeper, then by path and line
    /// so the result never depends on filesystem iteration order.
    pub fn definitions(&self, name: &str) -> Vec<SymbolRef<'_>> {
        let Some(hits) = self.by_name.get(&name.to_lowercase()) else {
            return Vec::new();
        };

        let mut refs: Vec<SymbolRef<'_>> = hits
            .iter()
            .filter_map(|(file_index, symbol_index)| self.symbol_ref(*file_index, *symbol_index))
            .collect();

        refs.sort_by(|a, b| {
            jump_priority(a.symbol.kind)
                .cmp(&jump_priority(b.symbol.kind))
                .then(a.symbol.depth.cmp(&b.symbol.depth))
                .then(a.path.cmp(b.path))
                .then(a.symbol.line.cmp(&b.symbol.line))
        });
        refs
    }

    /// Fuzzy-search symbol names, best match first, capped at `limit`.
    ///
    /// An empty query returns nothing rather than the whole repository — an
    /// unfiltered dump of 50,000 symbols is never what the caller wanted.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SymbolRef<'_>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let needle = LoweredNeedle::new(query);
        #[cfg(test)]
        self.search_comparisons.store(0, Ordering::Relaxed);

        let mut last_search = self
            .last_search
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(cached) = last_search
            .as_ref()
            .filter(|cached| cached.needle == needle && cached.limit == limit)
        {
            return cached
                .hits
                .iter()
                .filter_map(|(file_index, symbol_index)| {
                    self.symbol_ref(*file_index, *symbol_index)
                })
                .collect();
        }

        // Three 20-pass release runs (limit 200), mean query-set totals:
        // repo/3,625 symbols — HEAP 1,621.702 us, SORT 1,579.293 us,
        // SELECT 1,450.499 us; target/100,000 — HEAP 21,848.295 us,
        // SORT 33,706.390 us, SELECT 21,746.457 us. SELECT wins at target
        // scale and is faster here, so partition the matches before sorting.
        // Do not reserve from the caller-controlled `limit`: the vector grows
        // only for actual matches, keeping `usize::MAX` safe.
        let mut candidates = Vec::new();
        for (file_index, (file, lowered_names)) in
            self.files.iter().zip(&self.lowered_names).enumerate()
        {
            for (symbol_index, (symbol, lowered_name)) in
                file.symbols.iter().zip(lowered_names).enumerate()
            {
                let lowered_name = lowered_name.as_deref().unwrap_or(&symbol.name);
                if let Some(score) = fuzzy_score_lowered(lowered_name, &needle) {
                    candidates.push(SearchCandidate {
                        score,
                        name_len: symbol.name.len(),
                        path: &file.path,
                        line: symbol.line,
                        file_index: file_index as u32,
                        symbol_index: symbol_index as u32,
                    });
                }
            }
        }
        #[cfg(test)]
        let compare = |a: &SearchCandidate, b: &SearchCandidate| {
            self.search_comparisons.fetch_add(1, Ordering::Relaxed);
            compare_search_candidates(a, b)
        };
        #[cfg(not(test))]
        let compare = compare_search_candidates;

        if candidates.len() > limit {
            candidates.select_nth_unstable_by(limit, &compare);
            candidates.truncate(limit);
        }
        // The partition discarded every candidate outside the top N, so this
        // full ordering never handles more than `limit` entries.
        candidates.sort_by(&compare);

        let hits: Vec<_> = candidates
            .into_iter()
            .map(|candidate| (candidate.file_index, candidate.symbol_index))
            .collect();
        let result = hits
            .iter()
            .filter_map(|(file_index, symbol_index)| self.symbol_ref(*file_index, *symbol_index))
            .collect();
        *last_search = Some(CachedSearch {
            needle,
            limit,
            hits,
        });
        result
    }

    fn symbol_ref(&self, file_index: u32, symbol_index: u32) -> Option<SymbolRef<'_>> {
        let file = self.files.get(file_index as usize)?;
        let symbol = file.symbols.get(symbol_index as usize)?;
        Some(SymbolRef {
            path: &file.path,
            symbol,
        })
    }
}

/// Result of walking one worker chunk.
struct ChunkOutcome {
    files: Vec<FileSymbols>,
    scanned: usize,
    stopped_early: bool,
}

/// Index one chunk of paths with a single reusable parser pool.
fn index_chunk(repo_root: &Path, paths: &[&String], cancel: &dyn CancelSignal) -> ChunkOutcome {
    let mut pool = ParserPool::new();
    let mut outcome = ChunkOutcome {
        files: Vec::new(),
        scanned: 0,
        stopped_early: false,
    };

    for path in paths {
        if cancel.is_cancelled() {
            outcome.stopped_early = true;
            return outcome;
        }
        #[cfg(test)]
        assert_ne!(
            path.as_str(),
            WORKER_PANIC_PROBE_PATH,
            "test-only symbol indexing worker panic probe"
        );
        outcome.scanned += 1;
        let Ok(source) = std::fs::read_to_string(repo_root.join(path.as_str())) else {
            continue;
        };
        let symbols = extract_symbols(&source, path, &mut pool);
        if symbols.is_empty() {
            continue;
        }
        outcome.files.push(FileSymbols {
            path: (*path).clone(),
            symbols,
        });
    }

    outcome
}

/// Ordering hint for "which definition did the user most likely mean".
fn jump_priority(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Type => 0,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Macro => 1,
        SymbolKind::Constant | SymbolKind::Module => 2,
        SymbolKind::Field | SymbolKind::Property | SymbolKind::Heading => 3,
    }
}

/// Score a lower-cased name against a lower-cased search needle.
///
/// `lowered_name` must already be lower-cased; `LoweredNeedle` guarantees the
/// same precondition for `needle`.
fn fuzzy_score_lowered(lowered_name: &str, needle: &LoweredNeedle) -> Option<i64> {
    let needle = needle.as_str();
    if needle.is_empty() {
        return None;
    }
    let length_penalty = lowered_name.chars().count() as i64;

    if lowered_name == needle {
        return Some(10_000 - length_penalty);
    }
    if lowered_name.starts_with(needle) {
        return Some(8_000 - length_penalty);
    }
    if let Some(position) = lowered_name.find(needle) {
        let boundary = lowered_name[..position]
            .chars()
            .next_back()
            .is_some_and(|c| c == '_' || c == '-' || c == '.' || c == ':');
        let base = if boundary { 6_000 } else { 4_000 };
        return Some(base - position as i64 - length_penalty);
    }

    subsequence_score(lowered_name, needle).map(|score| score - length_penalty)
}

/// Score a scattered-subsequence match (`fdc` matching `find_diff_cache`).
///
/// Returns `None` unless every needle character appears in order.
fn subsequence_score(haystack: &str, needle: &str) -> Option<i64> {
    let mut haystack_chars = haystack.chars().enumerate().peekable();
    let mut gaps: i64 = 0;
    let mut previous: Option<usize> = None;

    for wanted in needle.chars() {
        loop {
            let (position, actual) = haystack_chars.next()?;
            if actual == wanted {
                if let Some(previous) = previous {
                    gaps += (position - previous - 1) as i64;
                }
                previous = Some(position);
                break;
            }
        }
    }

    Some(2_000 - gaps.min(1_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    /// Fires once it has been polled `limit` times — makes "how much work did a
    /// cancelled build do" an exact, race-free assertion.
    struct PollCountCancel {
        limit: usize,
        polls: AtomicUsize,
    }

    impl CancelSignal for PollCountCancel {
        fn is_cancelled(&self) -> bool {
            self.polls.fetch_add(1, Ordering::SeqCst) >= self.limit
        }
    }

    fn outline(source: &str, filename: &str) -> Vec<(String, SymbolKind, usize, usize)> {
        let mut pool = ParserPool::new();
        extract_symbols(source, filename, &mut pool)
            .into_iter()
            .map(|s| (s.name, s.kind, s.line, s.depth))
            .collect()
    }

    fn names(source: &str, filename: &str) -> Vec<String> {
        let mut pool = ParserPool::new();
        extract_symbols(source, filename, &mut pool)
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    // ===== every language advertising a tags query must compile it =====

    #[test]
    fn test_all_tags_queries_compile() {
        let mut pool = ParserPool::new();
        for lang in SupportedLanguage::all() {
            if lang.tags_query().is_none() {
                continue;
            }
            assert!(
                pool.get_or_create_tags_query(lang).is_some(),
                "tags query for {lang:?} failed to compile"
            );
        }
    }

    #[test]
    fn test_languages_without_tags_query_are_intentional() {
        let unsupported: Vec<_> = SupportedLanguage::all()
            .filter(|lang| lang.tags_query().is_none())
            .collect();
        assert_eq!(
            unsupported,
            vec![
                SupportedLanguage::Svelte,
                SupportedLanguage::Vue,
                SupportedLanguage::Css,
                SupportedLanguage::MarkdownInline,
            ]
        );
    }

    // ===== per-language extraction =====

    #[test]
    fn test_extract_rust_outline() {
        let source = "\
pub struct Config {
    pub name: String,
}

impl Config {
    pub fn new() -> Self {
        Self { name: String::new() }
    }
}

fn helper() {}
";
        insta::assert_debug_snapshot!(outline(source, "src/config.rs"), @r#"
        [
            (
                "Config",
                Class,
                1,
                0,
            ),
            (
                "new",
                Method,
                6,
                0,
            ),
            (
                "helper",
                Function,
                11,
                0,
            ),
        ]
        "#);
    }

    #[test]
    fn test_extract_typescript() {
        let source = "\
export interface Props { a: number }
export class Widget {
  render(): void {}
}
export function setup() {}
";
        assert_eq!(
            names(source, "src/widget.ts"),
            vec!["Props", "Widget", "render", "setup"]
        );
    }

    #[test]
    fn test_extract_python() {
        let source = "\
class Loader:
    def load(self):
        pass

def main():
    pass
";
        insta::assert_debug_snapshot!(outline(source, "loader.py"), @r#"
        [
            (
                "Loader",
                Class,
                1,
                0,
            ),
            (
                "load",
                Function,
                2,
                1,
            ),
            (
                "main",
                Function,
                5,
                0,
            ),
        ]
        "#);
    }

    #[test]
    fn test_extract_go() {
        let source = "\
package main

type Config struct{}

func (c *Config) Load() {}

func main() {}
";
        assert_eq!(
            names(source, "main.go"),
            vec!["Config", "Load", "main"],
            "go tags should yield the type, its method and the free function"
        );
    }

    #[test]
    fn test_extract_c_sharp_uses_bundled_query() {
        let source = "\
namespace Demo {
  public class Service {
    public void Run() {}
    public int Count { get; set; }
  }
  interface IThing {}
}
";
        assert_eq!(
            names(source, "Service.cs"),
            vec!["Demo", "Service", "Run", "Count", "IThing"]
        );
    }

    #[test]
    fn test_extract_zig_uses_bundled_query() {
        let source = "\
const Point = struct { x: i32 };
pub fn add(a: i32, b: i32) i32 { return a + b; }
";
        let names = names(source, "point.zig");
        assert!(names.contains(&"Point".to_string()), "got {names:?}");
        assert!(names.contains(&"add".to_string()), "got {names:?}");
    }

    #[test]
    fn test_extract_bash_skips_function_local_assignments() {
        let source = "\
TOP_LEVEL=1

deploy() {
  local_var=2
  echo hi
}
";
        assert_eq!(names(source, "deploy.sh"), vec!["TOP_LEVEL", "deploy"]);
    }

    #[test]
    fn test_extract_markdown_headings() {
        let source = "\
# Title

intro

## Usage

### Options
";
        insta::assert_debug_snapshot!(outline(source, "README.md"), @r#"
        [
            (
                "Title",
                Heading,
                1,
                0,
            ),
            (
                "Usage",
                Heading,
                5,
                1,
            ),
            (
                "Options",
                Heading,
                7,
                2,
            ),
        ]
        "#);
    }

    // ===== edge cases as use cases =====

    #[test]
    fn test_empty_source_yields_no_symbols() {
        assert!(names("", "src/lib.rs").is_empty());
    }

    #[test]
    fn test_unsupported_extension_yields_no_symbols() {
        assert!(names("fn main() {}", "notes.txt").is_empty());
        assert!(names("body { color: red }", "site.css").is_empty());
    }

    #[test]
    fn test_file_without_extension_yields_no_symbols() {
        assert!(names("fn main() {}", "Makefile").is_empty());
    }

    #[test]
    fn test_syntax_errors_still_yield_recovered_symbols() {
        // tree-sitter error recovery keeps the well-formed prefix usable.
        let source = "fn ok() {}\nfn broken( {\n";
        let names = names(source, "src/broken.rs");
        assert!(names.contains(&"ok".to_string()), "got {names:?}");
    }

    #[test]
    fn test_cjk_identifier_is_extracted_with_location() {
        let source = "// 日本語のコメント\nfn 名前() {}\n";
        let mut pool = ParserPool::new();
        let symbols = extract_symbols(source, "src/cjk.rs", &mut pool);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "名前");
        assert_eq!(symbols[0].line, 2);
        // "fn " is three characters — a byte column would report 3 as well, so
        // assert the far more interesting case below.
        assert_eq!(symbols[0].column, 3);
    }

    #[test]
    fn test_column_after_multibyte_prefix_is_in_characters() {
        let source = "class 構造体 { メソッド() {} }\n";
        let mut pool = ParserPool::new();
        let symbols = extract_symbols(source, "src/cjk.ts", &mut pool);
        let method = symbols
            .iter()
            .find(|s| s.name == "メソッド")
            .expect("method symbol");
        // A byte column would report 18 here; characters are what the diff
        // renderer and the jump stack both count in.
        assert_eq!(method.column, "class 構造体 { ".chars().count());
        assert_eq!(method.column, 12);
        assert_eq!("class 構造体 { ".len(), 18);
    }

    #[test]
    fn test_supports_symbols() {
        assert!(supports_symbols("src/main.rs"));
        assert!(supports_symbols("README.md"));
        assert!(!supports_symbols("style.css"));
        assert!(!supports_symbols("data.json"));
    }

    // ===== index =====

    fn sample_index() -> SymbolIndex {
        SymbolIndex::from_files(vec![
            FileSymbols {
                path: "src/app.rs".to_string(),
                symbols: vec![
                    Symbol {
                        name: "App".to_string(),
                        kind: SymbolKind::Class,
                        line: 10,
                        column: 0,
                        depth: 0,
                    },
                    Symbol {
                        name: "render_app".to_string(),
                        kind: SymbolKind::Function,
                        line: 20,
                        column: 0,
                        depth: 0,
                    },
                ],
            },
            FileSymbols {
                path: "src/ui.rs".to_string(),
                symbols: vec![Symbol {
                    name: "app".to_string(),
                    kind: SymbolKind::Constant,
                    line: 5,
                    column: 0,
                    depth: 0,
                }],
            },
        ])
    }

    fn test_symbol(name: &str, kind: SymbolKind, line: usize, column: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            line,
            column,
            depth: 0,
        }
    }

    fn completed_index(build: IndexBuild) -> SymbolIndex {
        match build {
            IndexBuild::Completed(index) => index,
            IndexBuild::Cancelled { scanned_files } => {
                panic!("build was cancelled after scanning {scanned_files} files")
            }
            IndexBuild::Failed { message } => panic!("build failed: {message}"),
        }
    }

    fn full_sort_search_reference<'a>(
        index: &'a SymbolIndex,
        query: &str,
        limit: usize,
    ) -> Vec<SymbolRef<'a>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let needle = LoweredNeedle::new(query);
        let mut scored = Vec::new();

        for file in &index.files {
            for symbol in &file.symbols {
                if let Some(score) = fuzzy_score_lowered(&symbol.name.to_lowercase(), &needle) {
                    scored.push((
                        score,
                        SymbolRef {
                            path: &file.path,
                            symbol,
                        },
                    ));
                }
            }
        }

        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(a.1.symbol.name.len().cmp(&b.1.symbol.name.len()))
                .then(a.1.path.cmp(b.1.path))
                .then(a.1.symbol.line.cmp(&b.1.symbol.line))
        });
        scored.truncate(limit);
        scored.into_iter().map(|(_, symbol)| symbol).collect()
    }

    #[test]
    fn test_index_counts() {
        let index = sample_index();
        assert_eq!(index.file_count(), 2);
        assert_eq!(index.symbol_count(), 3);
        assert!(!index.is_empty());
    }

    #[test]
    fn test_empty_index_is_empty() {
        let index = SymbolIndex::default();
        assert!(index.is_empty());
        assert_eq!(index.symbol_count(), 0);
        assert!(index.definitions("anything").is_empty());
        assert!(index.search("anything", 10).is_empty());
    }

    #[test]
    fn test_definitions_are_case_insensitive_and_ranked() {
        let index = sample_index();
        let hits = index.definitions("APP");
        let rendered: Vec<_> = hits
            .iter()
            .map(|hit| (hit.path, hit.symbol.name.as_str(), hit.symbol.kind))
            .collect();
        // The class outranks the constant regardless of insertion order.
        assert_eq!(
            rendered,
            vec![
                ("src/app.rs", "App", SymbolKind::Class),
                ("src/ui.rs", "app", SymbolKind::Constant),
            ]
        );
    }

    #[test]
    fn test_definitions_unknown_name() {
        assert!(sample_index().definitions("nope").is_empty());
    }

    #[test]
    fn test_file_symbols_lookup() {
        let index = sample_index();
        assert_eq!(index.file_symbols("src/ui.rs").map(|s| s.len()), Some(1));
        assert!(index.file_symbols("src/missing.rs").is_none());
    }

    #[test]
    fn test_search_prefers_exact_over_boundary_match() {
        let index = sample_index();
        let hits = index.search("app", 10);
        let names: Vec<_> = hits.iter().map(|hit| hit.symbol.name.as_str()).collect();
        assert_eq!(names, vec!["App", "app", "render_app"]);
    }

    #[test]
    fn test_search_respects_limit() {
        assert_eq!(sample_index().search("app", 1).len(), 1);
        assert!(sample_index().search("app", 0).is_empty());
    }

    #[test]
    fn test_search_with_an_unbounded_limit_returns_every_match() {
        let index = sample_index();
        let expected = index.search("app", index.symbol_count());

        assert_eq!(index.search("app", usize::MAX), expected);
        assert_eq!(index.search("app", index.symbol_count() * 1_000), expected);
    }

    #[test]
    fn test_search_empty_query_returns_nothing() {
        assert!(sample_index().search("", 10).is_empty());
        assert!(sample_index().search("   ", 10).is_empty());
    }

    #[test]
    fn test_search_subsequence() {
        let index = sample_index();
        let hits = index.search("rndap", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol.name, "render_app");
    }

    #[test]
    fn test_search_reads_a_matching_cached_result() {
        let index = sample_index();
        *index.last_search.lock().unwrap() = Some(CachedSearch {
            needle: LoweredNeedle::new("app"),
            limit: 10,
            hits: vec![(0, 1)],
        });

        let hits = index.search("app", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol.name, "render_app");
    }

    /// Ties are broken all the way down to insertion order.
    ///
    /// The top-N partition is an *unstable* selection, so candidates the
    /// comparator calls equal may be kept, dropped and ordered differently
    /// between runs. Every user-visible key can genuinely tie — same score,
    /// same name length, same file, same line describes overloads on one line
    /// and macro-generated pairs — and only the index tie-breakers make the
    /// result reproducible.
    #[test]
    fn test_candidates_equal_on_every_visible_key_still_have_one_order() {
        const TOTAL: usize = 300;
        const LIMIT: usize = 200;
        // Equal-length names on one line in one file: score, name length, path
        // and line are all identical across the set.
        let symbols = (0..TOTAL)
            .map(|index| test_symbol(&format!("tie_{index:04}"), SymbolKind::Function, 1, 0))
            .collect();
        let index = SymbolIndex::from_files(vec![FileSymbols {
            path: "src/generated.rs".to_string(),
            symbols,
        }]);

        let hits = index.search("tie", LIMIT);

        assert_eq!(hits.len(), LIMIT);
        let names: Vec<&str> = hits.iter().map(|hit| hit.symbol.name.as_str()).collect();
        let expected: Vec<String> = (0..LIMIT).map(|index| format!("tie_{index:04}")).collect();
        assert_eq!(
            names, expected,
            "candidates that tie on every visible key must still come back in \
             index order; without it the unstable partition keeps a different \
             subset each run"
        );
    }

    /// A poisoned search cache is recovered from, not propagated.
    ///
    /// The scoring pass and the unstable partition both run while the cache
    /// lock is held, so any panic there poisons it. Taking the lock with
    /// `unwrap()` would then turn one panic into a panic on every subsequent
    /// keystroke in the symbol-search overlay.
    #[test]
    fn test_a_poisoned_search_cache_does_not_break_every_later_search() {
        let index = std::sync::Arc::new(sample_index());
        assert!(!index.search("app", 10).is_empty());

        let poisoner = std::sync::Arc::clone(&index);
        let panicked = std::thread::spawn(move || {
            let _guard = poisoner.last_search.lock().unwrap();
            panic!("poison the search cache");
        })
        .join();
        assert!(
            panicked.is_err(),
            "the fixture must actually poison the lock"
        );

        assert!(
            !index.search("app", 10).is_empty(),
            "one panic must not take every later symbol search with it"
        );
    }

    #[test]
    fn test_search_never_orders_more_than_the_limit() {
        // Scrambled, not ascending. Candidates are collected in index order, so
        // ascending lines would arrive already in final order and Rust's sort
        // would finish in a single linear run — making a full sort *cheaper*
        // than the partition and hiding the very substitution this test exists
        // to catch. 7,919 is coprime with 5,000, so this is a permutation.
        let symbols = (0..5_000)
            .map(|index| {
                test_symbol(
                    &format!("matching_symbol_{index:04}"),
                    SymbolKind::Function,
                    (index * 7_919) % 5_000 + 1,
                    0,
                )
            })
            .collect();
        let index = SymbolIndex::from_files(vec![FileSymbols {
            path: "src/generated.rs".to_string(),
            symbols,
        }]);

        assert_eq!(index.symbol_count(), 5_000);
        assert_eq!(index.search("matching", usize::MAX).len(), 5_000);

        let sorted = index.search("matching", 10);
        assert_eq!(sorted.len(), 10);

        // Ordering *work*, not a number recorded beside it: a probe reporting
        // the post-truncate candidate count reads `limit` either way, so moving
        // the truncate above it hides a full sort. Comparisons cannot be
        // reordered around — top-N partitioning is linear in the match count
        // while a full sort of 5,000 unordered candidates costs n log n.
        let comparisons = index.search_comparisons();
        assert!(
            comparisons < 25_000,
            "ordering 10 of 5,000 matches took {comparisons} comparisons; \
             the top-N partition looks to have been replaced by a full sort"
        );
    }

    #[test]
    fn test_search_matches_a_full_sort_reference() {
        let index = SymbolIndex::from_files(vec![
            FileSymbols {
                path: "src/z.rs".to_string(),
                symbols: vec![
                    test_symbol("sym", SymbolKind::Function, 50, 0),
                    test_symbol("symé", SymbolKind::Function, 40, 0),
                    test_symbol("syma", SymbolKind::Function, 40, 0),
                    test_symbol("sym_a", SymbolKind::Function, 12, 0),
                    test_symbol("sym_b", SymbolKind::Method, 12, 0),
                    test_symbol("sym_a", SymbolKind::Constant, 12, 4),
                    test_symbol("sym_long", SymbolKind::Function, 2, 0),
                ],
            },
            FileSymbols {
                path: "src/a.rs".to_string(),
                symbols: vec![
                    test_symbol("sym_c", SymbolKind::Function, 30, 0),
                    test_symbol("sym_d", SymbolKind::Function, 10, 0),
                    test_symbol("do_sym", SymbolKind::Function, 1, 0),
                ],
            },
            FileSymbols {
                path: "src/a.rs".to_string(),
                symbols: vec![
                    test_symbol("sym_e", SymbolKind::Method, 10, 2),
                    test_symbol("asym", SymbolKind::Class, 5, 0),
                ],
            },
        ]);

        for limit in [1, 2, 5, 9, 20] {
            let expected = full_sort_search_reference(&index, "sym", limit);
            assert_eq!(index.search("sym", limit), expected, "limit {limit}");
        }
    }

    #[test]
    fn test_search_cache_is_keyed_by_needle_and_limit() {
        let index = sample_index();
        *index.last_search.lock().unwrap() = Some(CachedSearch {
            needle: LoweredNeedle::new("app"),
            limit: 1,
            hits: vec![(0, 1)],
        });
        let names: Vec<_> = index
            .search("app", 10)
            .iter()
            .map(|hit| hit.symbol.name.as_str())
            .collect();
        assert_eq!(names, vec!["App", "app", "render_app"]);

        *index.last_search.lock().unwrap() = Some(CachedSearch {
            needle: LoweredNeedle::new("render"),
            limit: 10,
            hits: vec![(0, 1)],
        });
        let names: Vec<_> = index
            .search("app", 10)
            .iter()
            .map(|hit| hit.symbol.name.as_str())
            .collect();
        assert_eq!(names, vec!["App", "app", "render_app"]);
    }

    #[test]
    fn test_search_writes_computed_results_to_cache() {
        let index = sample_index();
        let hits = index.search("app", 10);
        let last_search = index.last_search.lock().unwrap();
        let cached = last_search
            .as_ref()
            .expect("computed search was not cached");

        assert_eq!(cached.needle.as_str(), "app");
        assert_eq!(cached.limit, 10);
        assert_eq!(cached.hits.len(), hits.len());
    }

    #[test]
    fn test_search_scores_against_the_precomputed_lowered_names() {
        let mut index = SymbolIndex::from_files(vec![FileSymbols {
            path: "src/zebra.rs".to_string(),
            symbols: vec![test_symbol("Zebra", SymbolKind::Function, 1, 0)],
        }]);
        index.lowered_names[0][0] = Some("apple".to_string().into_boxed_str());

        let hits = index.search("apple", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol.name, "Zebra");
        assert!(index.search("zebra", 10).is_empty());
    }

    #[test]
    fn test_search_results_are_unchanged_by_precomputed_lowercasing() {
        let names = ["MixedCase", "UPPERCASE", "名前", "İ", "ß", "Éclair"];
        let symbols = names
            .iter()
            .enumerate()
            .map(|(index, name)| test_symbol(name, SymbolKind::Function, index + 1, 0))
            .collect();
        let index = SymbolIndex::from_files(vec![FileSymbols {
            path: "src/unicode.rs".to_string(),
            symbols,
        }]);

        for query in ["case", "名前", "i", "ß", "é", "air", "xyz"] {
            let expected = full_sort_search_reference(&index, query, names.len());
            assert_eq!(
                index.search(query, names.len()),
                expected,
                "query {query:?}"
            );
        }
    }

    #[test]
    fn test_search_is_case_insensitive_for_queries() {
        let names = ["MixedCase", "UPPERCASE", "名前", "İ", "ß", "Éclair"];
        let symbols = names
            .iter()
            .enumerate()
            .map(|(index, name)| test_symbol(name, SymbolKind::Function, index + 1, 0))
            .collect();
        let index = SymbolIndex::from_files(vec![FileSymbols {
            path: "src/unicode.rs".to_string(),
            symbols,
        }]);

        for query in ["MiXeDcAsE", "uPpErCaSe", "名前", "İ", "ß", "ÉcLaIr"] {
            let result = index.search(query, names.len());
            let expected = full_sort_search_reference(&index, query, names.len());
            assert_eq!(
                result, expected,
                "query {query:?} differs from the independent reference"
            );
            assert_eq!(
                result,
                index.search(&query.to_lowercase(), names.len()),
                "query {query:?} differs from its lowercase form"
            );
            assert!(!result.is_empty(), "query {query:?} must match");
        }
    }

    #[test]
    fn test_symbol_index_is_send_and_sync() {
        fn assert_send_and_sync<T: Send + Sync>() {}

        assert_send_and_sync::<SymbolIndex>();
    }

    #[test]
    fn test_symbol_ref_search_label() {
        let cases = [
            (
                test_symbol("alpha", SymbolKind::Function, 3, 0),
                "src/a.rs",
                "ƒ alpha  src/a.rs:3",
            ),
            (
                test_symbol("名前", SymbolKind::Method, 27, 4),
                "src/parser/names.rs",
                "m 名前  src/parser/names.rs:27",
            ),
            (
                test_symbol("Configuration", SymbolKind::Class, 91, 0),
                "crates/core/src/config.rs",
                "C Configuration  crates/core/src/config.rs:91",
            ),
        ];

        for (symbol, path, expected) in &cases {
            assert_eq!(SymbolRef { path, symbol }.search_label(), *expected);
        }
    }

    #[test]
    fn test_browse_symbol_search_results_match_symbol_ref_search_label() {
        let mut nested = test_symbol("target_nested", SymbolKind::Method, 27, 4);
        nested.depth = 2;
        let index = std::sync::Arc::new(SymbolIndex::from_files(vec![
            FileSymbols {
                path: "src/search.rs".to_string(),
                symbols: vec![
                    test_symbol("target_function", SymbolKind::Function, 3, 0),
                    nested,
                ],
            },
            FileSymbols {
                path: "src/検索/names.rs".to_string(),
                symbols: vec![test_symbol("検索Target", SymbolKind::Class, 91, 0)],
            },
        ]));
        let mut state = crate::app::browse::BrowseState::new(
            std::path::PathBuf::from("/repo"),
            crate::app::AppState::FileList,
        );
        state.index = crate::app::browse::IndexState::Ready(std::sync::Arc::clone(&index));

        let query = "target";
        let expected = index.search(query, usize::MAX);
        let actual = state.symbol_search_results(query);

        assert_eq!(actual.len(), expected.len());
        for ((path, line, label), hit) in actual.iter().zip(expected) {
            assert_eq!(label, &hit.search_label());
            assert_eq!(path, hit.path);
            assert_eq!(*line, hit.symbol.line);
        }
    }

    // ===== fuzzy scoring =====

    #[test]
    fn test_fuzzy_score_tiers() {
        let needle = LoweredNeedle::new("parse");
        let exact = fuzzy_score_lowered("parse", &needle).unwrap();
        let prefix = fuzzy_score_lowered("parse_line", &needle).unwrap();
        let boundary = fuzzy_score_lowered("do_parse", &needle).unwrap();
        let middle = fuzzy_score_lowered("reparsed", &needle).unwrap();
        let scattered = fuzzy_score_lowered("please_advance_rest_of_set", &needle).unwrap();

        assert!(exact > prefix, "{exact} > {prefix}");
        assert!(prefix > boundary, "{prefix} > {boundary}");
        assert!(boundary > middle, "{boundary} > {middle}");
        assert!(middle > scattered, "{middle} > {scattered}");
    }

    #[test]
    fn test_fuzzy_score_no_match() {
        assert!(fuzzy_score_lowered("parse", &LoweredNeedle::new("xyz")).is_none());
        assert!(fuzzy_score_lowered("parse", &LoweredNeedle::new("")).is_none());
    }

    #[test]
    fn test_fuzzy_score_shorter_name_wins_within_tier() {
        let needle = LoweredNeedle::new("parse");
        let short = fuzzy_score_lowered("parse_a", &needle).unwrap();
        let long = fuzzy_score_lowered("parse_a_very_long_name", &needle).unwrap();
        assert!(short > long);
    }

    #[test]
    fn test_fuzzy_score_is_case_insensitive() {
        let mixed_case = fuzzy_score_lowered("parse", &LoweredNeedle::new("PaRsE"));
        let lower_case = fuzzy_score_lowered("parse", &LoweredNeedle::new("parse"));

        assert_eq!(mixed_case, lower_case);
        assert!(mixed_case.is_some());
    }

    #[test]
    fn test_fuzzy_score_is_case_insensitive_for_tricky_names() {
        let names = [
            "lowercase",
            "UPPERCASE",
            "MixedCase",
            "identifier_123_name",
            "名前",
            "ǅ",
            "İ",
            "ß",
            "Éclair",
        ];
        let needle_pairs = [
            ("LoWeR", "lower"),
            ("UpPeR", "upper"),
            ("MiXeD", "mixed"),
            ("Identifier_123", "identifier_123"),
            ("ǅ", "ǆ"),
            ("İ", "i\u{307}"),
            ("ẞ", "ß"),
            ("ÉcLaIr", "éclair"),
            ("XyZ", "xyz"),
        ];

        for (mixed, lower) in needle_pairs {
            assert_eq!(
                mixed.to_lowercase(),
                lower,
                "invalid mixed/lower pair: {mixed:?}, {lower:?}"
            );
            let mixed_needle = LoweredNeedle::new(mixed);
            let lower_needle = LoweredNeedle::new(lower);
            for name in names {
                let lowered_name = name.to_lowercase();
                assert_eq!(
                    fuzzy_score_lowered(&lowered_name, &mixed_needle),
                    fuzzy_score_lowered(&lowered_name, &lower_needle),
                    "name={name:?}, mixed={mixed:?}, lower={lower:?}"
                );
            }
        }
    }

    // ===== index build over a real directory =====

    #[test]
    fn test_cancelled_build_stops_scanning_early() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("src");
        std::fs::create_dir(&source_dir).unwrap();
        let paths: Vec<_> = (0..1_000)
            .map(|index| {
                let path = format!("src/file_{index:04}.rs");
                std::fs::write(dir.path().join(&path), format!("pub fn f{index}() {{}}\n"))
                    .unwrap();
                path
            })
            .collect();

        let completed =
            SymbolIndex::build_cancellable(dir.path(), &paths, &CancellationToken::new());
        let control_scanned = match completed {
            IndexBuild::Completed(index) => index.scanned_file_count(),
            other => panic!("control build did not complete: {other:?}"),
        };
        assert_eq!(control_scanned, 1_000);

        let cancel = PollCountCancel {
            limit: 50,
            polls: AtomicUsize::new(0),
        };
        match SymbolIndex::build_cancellable(dir.path(), &paths, &cancel) {
            IndexBuild::Cancelled { scanned_files } => {
                assert!(scanned_files <= 64, "scanned {scanned_files} files");
                assert!(scanned_files < control_scanned);
                assert!(scanned_files < 1_000);
            }
            IndexBuild::Completed(index) => panic!(
                "cancelled build completed after scanning {} files",
                index.scanned_file_count()
            ),
            IndexBuild::Failed { message } => {
                panic!("cancelled build failed instead of stopping early: {message}")
            }
        }
    }

    #[test]
    fn test_cancelled_build_stops_the_metadata_prefilter() {
        let dir = tempfile::tempdir().unwrap();
        // Deliberately non-indexable paths leave the pre-filter as the only
        // work, so cancellation can only come from a poll inside that loop.
        let paths: Vec<_> = (0..5_000)
            .map(|index| format!("notes/file_{index:04}.txt"))
            .collect();
        let cancel = PollCountCancel {
            limit: 1,
            polls: AtomicUsize::new(0),
        };

        match SymbolIndex::build_cancellable(dir.path(), &paths, &cancel) {
            IndexBuild::Cancelled { scanned_files } => assert_eq!(scanned_files, 0),
            IndexBuild::Completed(_) => {
                panic!("metadata pre-filter ignored cancellation")
            }
            IndexBuild::Failed { message } => {
                panic!("metadata pre-filter failed instead of cancelling: {message}")
            }
        }
    }

    #[test]
    fn test_precancelled_build_scans_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.rs"), "pub fn present() {}\n").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        match SymbolIndex::build_cancellable(dir.path(), &["file.rs".to_string()], &cancel) {
            IndexBuild::Cancelled { scanned_files } => assert_eq!(scanned_files, 0),
            other => panic!("pre-cancelled build did not cancel: {other:?}"),
        }
    }

    #[test]
    fn test_precancelled_build_with_no_indexable_paths_is_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        // Keep this below the pre-filter poll interval and non-indexable:
        // otherwise a later pre-filter or worker poll would mask removal of the
        // pre-build cancellation short-circuit.
        let paths = ["notes.txt".to_string()];

        match SymbolIndex::build_cancellable(dir.path(), &paths, &cancel) {
            IndexBuild::Cancelled { scanned_files } => assert_eq!(scanned_files, 0),
            other => panic!("pre-cancelled empty build did not cancel: {other:?}"),
        }
    }

    #[test]
    fn test_build_over_a_missing_root_fails() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");

        match SymbolIndex::build_cancellable(&missing, &[], &CancellationToken::new()) {
            IndexBuild::Failed { message } => {
                assert!(message.contains(&missing.to_string_lossy().into_owned()));
            }
            other => panic!("missing root did not fail: {other:?}"),
        }
    }

    #[test]
    fn test_completed_build_reports_every_walked_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("symbols.rs"), "pub fn present() {}\n").unwrap();
        std::fs::write(dir.path().join("comments.rs"), "// no symbols here\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "unsupported\n").unwrap();
        let paths = vec![
            "symbols.rs".to_string(),
            "comments.rs".to_string(),
            "notes.txt".to_string(),
            "missing.rs".to_string(),
        ];

        match SymbolIndex::build_cancellable(dir.path(), &paths, &CancellationToken::new()) {
            IndexBuild::Completed(index) => {
                assert_eq!(index.file_count(), 1);
                assert_eq!(index.scanned_file_count(), 2);
            }
            other => panic!("build did not complete: {other:?}"),
        }
    }

    #[test]
    fn test_build_indexes_supported_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("src/b.ts"), "export function beta() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not code\n").unwrap();

        let paths = vec![
            "src/a.rs".to_string(),
            "src/b.ts".to_string(),
            "notes.txt".to_string(),
            "src/missing.rs".to_string(),
        ];
        let index = completed_index(SymbolIndex::build_cancellable(
            dir.path(),
            &paths,
            &CancellationToken::new(),
        ));

        assert_eq!(index.file_count(), 2);
        assert_eq!(index.symbol_count(), 2);
        assert_eq!(index.definitions("alpha").len(), 1);
        assert_eq!(index.definitions("beta")[0].path, "src/b.ts");
    }

    #[test]
    fn test_build_fails_when_index_worker_panics() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut paths = Vec::new();
        for (path, source) in [
            ("src/ordinary_a.rs", "pub fn ordinary_a() {}\n"),
            ("src/ordinary_b.rs", "pub fn ordinary_b() {}\n"),
            ("src/ordinary_c.rs", "pub fn ordinary_c() {}\n"),
        ] {
            std::fs::write(dir.path().join(path), source).unwrap();
            paths.push(path.to_string());
        }
        std::fs::write(
            dir.path().join(WORKER_PANIC_PROBE_PATH),
            "pub fn panic_probe() {}\n",
        )
        .unwrap();
        assert!(supports_symbols(WORKER_PANIC_PROBE_PATH));
        assert!(
            std::fs::metadata(dir.path().join(WORKER_PANIC_PROBE_PATH))
                .unwrap()
                .len()
                <= MAX_INDEXED_FILE_BYTES
        );
        paths.push(WORKER_PANIC_PROBE_PATH.to_string());

        match SymbolIndex::build_cancellable(dir.path(), &paths, &CancellationToken::new()) {
            IndexBuild::Completed(index) => panic!(
                "worker panic degraded into a completed short index with {} indexed files",
                index.file_count()
            ),
            IndexBuild::Failed { message } => {
                assert!(
                    message.contains("symbol indexing worker panicked"),
                    "unexpected failure: {message}"
                );
                assert!(
                    message.contains(&dir.path().display().to_string()),
                    "failure did not name repository root: {message}"
                );
                assert!(!message.contains("is unavailable"), "{message}");
                assert!(!message.contains("is not a directory"), "{message}");
            }
            IndexBuild::Cancelled { scanned_files } => {
                panic!("worker panic was reported as cancellation after {scanned_files} files")
            }
        }
    }

    /// A repository root that is not a directory is a failure, not an empty index.
    ///
    /// Without the check every `repo_root.join(path)` simply fails to stat, so
    /// the walk finishes and reports `Completed` with nothing in it. The user
    /// then gets a silently empty symbol index and no banner explaining why —
    /// the one outcome `IndexBuild::Failed` exists to prevent.
    #[test]
    fn test_a_repository_root_that_is_a_file_fails_instead_of_indexing_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_directory = dir.path().join("root.rs");
        std::fs::write(&not_a_directory, "pub fn a() {}\n").unwrap();

        let build = SymbolIndex::build_cancellable(
            &not_a_directory,
            &["src/lib.rs".to_string()],
            &CancellationToken::new(),
        );

        match build {
            IndexBuild::Failed { message } => {
                assert!(message.contains("is not a directory"), "{message}");
                assert!(
                    message.contains(&not_a_directory.display().to_string()),
                    "{message}"
                );
            }
            IndexBuild::Completed(index) => panic!(
                "a non-directory root degraded into an empty index with {} symbols",
                index.symbol_count()
            ),
            IndexBuild::Cancelled { .. } => panic!("nothing cancelled this build"),
        }
    }

    #[test]
    fn test_build_with_no_paths() {
        let dir = tempfile::tempdir().unwrap();
        let index = completed_index(SymbolIndex::build_cancellable(
            dir.path(),
            &[],
            &CancellationToken::new(),
        ));
        assert!(index.is_empty());
        assert_eq!(index.file_count(), 0);
    }

    #[test]
    fn test_build_skips_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let huge = format!(
            "pub fn big() {{}}\n{}",
            "// filler filler filler filler\n".repeat(80_000)
        );
        assert!(huge.len() as u64 > MAX_INDEXED_FILE_BYTES);
        std::fs::write(dir.path().join("big.rs"), &huge).unwrap();
        std::fs::write(dir.path().join("small.rs"), "pub fn small() {}\n").unwrap();

        let index = completed_index(SymbolIndex::build_cancellable(
            dir.path(),
            &["big.rs".to_string(), "small.rs".to_string()],
            &CancellationToken::new(),
        ));
        assert_eq!(index.definitions("small").len(), 1);
        assert!(index.file_symbols("small.rs").is_some());
        assert!(index.definitions("big").is_empty());
        assert!(index.file_symbols("big.rs").is_none());
    }
}
