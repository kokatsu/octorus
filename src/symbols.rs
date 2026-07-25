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

    /// Human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Module => "module",
            Self::Macro => "macro",
            Self::Constant => "constant",
            Self::Type => "type",
            Self::Field => "field",
            Self::Property => "property",
            Self::Heading => "heading",
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

/// A repository-wide symbol index.
///
/// Built once per browse session from a list of repository-relative paths and
/// queried synchronously afterwards.
#[derive(Debug, Default)]
pub struct SymbolIndex {
    files: Vec<FileSymbols>,
    /// Lower-cased name -> (file index, symbol index) pairs.
    by_name: HashMap<String, SmallVec<[(u32, u32); 2]>>,
    /// Number of files that were walked, including ones with no symbols.
    scanned_files: usize,
}

impl SymbolIndex {
    /// Build an index from already-extracted per-file symbols.
    pub fn from_files(files: Vec<FileSymbols>) -> Self {
        let mut by_name: HashMap<String, SmallVec<[(u32, u32); 2]>> = HashMap::new();
        let scanned_files = files.len();

        for (file_index, file) in files.iter().enumerate() {
            for (symbol_index, symbol) in file.symbols.iter().enumerate() {
                by_name
                    .entry(symbol.name.to_lowercase())
                    .or_default()
                    .push((file_index as u32, symbol_index as u32));
            }
        }

        Self {
            files,
            by_name,
            scanned_files,
        }
    }

    /// Build an index by reading and parsing every given repository-relative path.
    ///
    /// Blocking and CPU-bound — call it from `spawn_blocking`, never from the
    /// render loop. Unreadable, oversized and unsupported files are skipped
    /// silently; a repository is allowed to contain files octorus cannot parse.
    pub fn build(repo_root: &Path, paths: &[String]) -> Self {
        let indexable: Vec<&String> = paths
            .iter()
            .filter(|path| supports_symbols(path))
            .filter(|path| {
                std::fs::metadata(repo_root.join(path.as_str()))
                    .map(|meta| meta.is_file() && meta.len() <= MAX_INDEXED_FILE_BYTES)
                    .unwrap_or(false)
            })
            .collect();

        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 8)
            .min(indexable.len().max(1));

        let chunk_size = indexable.len().div_ceil(workers).max(1);
        let mut files: Vec<FileSymbols> = std::thread::scope(|scope| {
            let handles: Vec<_> = indexable
                .chunks(chunk_size)
                .map(|chunk| scope.spawn(move || index_chunk(repo_root, chunk)))
                .collect();

            handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .flatten()
                .collect()
        });

        // Chunks finish out of order; a stable path order keeps search results
        // and snapshots deterministic.
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Self::from_files(files)
    }

    /// Number of indexed files that contributed at least one symbol.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Number of files walked while building the index.
    pub fn scanned_file_count(&self) -> usize {
        self.scanned_files
    }

    /// Total number of indexed symbols.
    pub fn symbol_count(&self) -> usize {
        self.files.iter().map(|file| file.symbols.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.symbol_count() == 0
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
        let needle = query.to_lowercase();

        let mut scored: Vec<(i64, SymbolRef<'_>)> = Vec::new();
        for file in &self.files {
            for symbol in &file.symbols {
                if let Some(score) = fuzzy_score(&symbol.name, &needle) {
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

    fn symbol_ref(&self, file_index: u32, symbol_index: u32) -> Option<SymbolRef<'_>> {
        let file = self.files.get(file_index as usize)?;
        let symbol = file.symbols.get(symbol_index as usize)?;
        Some(SymbolRef {
            path: &file.path,
            symbol,
        })
    }
}

/// Index one chunk of paths with a single reusable parser pool.
fn index_chunk(repo_root: &Path, paths: &[&String]) -> Vec<FileSymbols> {
    let mut pool = ParserPool::new();
    let mut result = Vec::new();

    for path in paths {
        let Ok(source) = std::fs::read_to_string(repo_root.join(path.as_str())) else {
            continue;
        };
        let symbols = extract_symbols(&source, path, &mut pool);
        if symbols.is_empty() {
            continue;
        }
        result.push(FileSymbols {
            path: (*path).clone(),
            symbols,
        });
    }

    result
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

/// Score `name` against an already lower-cased `needle`.
///
/// Higher is better; `None` means no match. The tiers are deliberately far
/// apart so an exact hit can never be outranked by a long subsequence match.
pub fn fuzzy_score(name: &str, needle: &str) -> Option<i64> {
    if needle.is_empty() {
        return None;
    }
    let lowered = name.to_lowercase();
    let length_penalty = lowered.chars().count() as i64;

    if lowered == needle {
        return Some(10_000 - length_penalty);
    }
    if lowered.starts_with(needle) {
        return Some(8_000 - length_penalty);
    }
    if let Some(position) = lowered.find(needle) {
        let boundary = lowered[..position]
            .chars()
            .next_back()
            .is_some_and(|c| c == '_' || c == '-' || c == '.' || c == ':');
        let base = if boundary { 6_000 } else { 4_000 };
        return Some(base - position as i64 - length_penalty);
    }

    subsequence_score(&lowered, needle).map(|score| score - length_penalty)
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
    fn test_extract_rust_outline_with_nesting() {
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
    fn test_cjk_identifier_column_is_in_characters() {
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
    fn test_search_prefers_exact_then_prefix() {
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

    // ===== fuzzy scoring =====

    #[test]
    fn test_fuzzy_score_tiers() {
        let exact = fuzzy_score("parse", "parse").unwrap();
        let prefix = fuzzy_score("parse_line", "parse").unwrap();
        let boundary = fuzzy_score("do_parse", "parse").unwrap();
        let middle = fuzzy_score("reparsed", "parse").unwrap();
        let scattered = fuzzy_score("please_advance_rest_of_set", "parse").unwrap();

        assert!(exact > prefix, "{exact} > {prefix}");
        assert!(prefix > boundary, "{prefix} > {boundary}");
        assert!(boundary > middle, "{boundary} > {middle}");
        assert!(middle > scattered, "{middle} > {scattered}");
    }

    #[test]
    fn test_fuzzy_score_no_match() {
        assert!(fuzzy_score("parse", "xyz").is_none());
        assert!(fuzzy_score("parse", "").is_none());
    }

    #[test]
    fn test_fuzzy_score_shorter_name_wins_within_tier() {
        let short = fuzzy_score("parse_a", "parse").unwrap();
        let long = fuzzy_score("parse_a_very_long_name", "parse").unwrap();
        assert!(short > long);
    }

    #[test]
    fn test_fuzzy_score_is_case_insensitive() {
        assert_eq!(fuzzy_score("Parse", "parse"), fuzzy_score("parse", "parse"));
    }

    // ===== index build over a real directory =====

    #[test]
    fn test_build_over_directory() {
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
        let index = SymbolIndex::build(dir.path(), &paths);

        assert_eq!(index.file_count(), 2);
        assert_eq!(index.symbol_count(), 2);
        assert_eq!(index.definitions("alpha").len(), 1);
        assert_eq!(index.definitions("beta")[0].path, "src/b.ts");
    }

    #[test]
    fn test_build_with_no_paths() {
        let dir = tempfile::tempdir().unwrap();
        let index = SymbolIndex::build(dir.path(), &[]);
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

        let index = SymbolIndex::build(dir.path(), &["big.rs".to_string()]);
        assert!(index.is_empty());
    }
}
