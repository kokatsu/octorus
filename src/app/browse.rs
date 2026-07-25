//! Repository Browser — read the whole repository, not just the diff.
//!
//! Every other screen in octorus is anchored to a change: a PR's changed files,
//! a local diff, a commit. Browse is the missing half — it walks the tracked
//! working tree, opens any file read-only with the same tree-sitter
//! highlighting the diff view uses, and navigates by symbol rather than by
//! filename.
//!
//! Symbol navigation is backed by [`crate::symbols::SymbolIndex`], built once
//! per browse session on a background thread. Until it is ready every other
//! part of the screen stays usable — an index is an accelerator, never a
//! prerequisite.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::filter::ListFilter;
use crate::symbols::{Symbol, SymbolIndex};
use crate::syntax::ParserPool;

use super::types::*;
use super::{App, AppState};

/// Upper bound on files listed in the browser.
///
/// A monorepo with a million tracked paths would spend more time building the
/// tree than the user will ever spend scrolling it.
pub const MAX_BROWSE_FILES: usize = 200_000;

/// Files larger than this are shown as a notice instead of being highlighted.
pub const MAX_VIEWABLE_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Maximum rows shown in the symbol search overlay.
pub const MAX_SYMBOL_SEARCH_RESULTS: usize = 200;

impl App {
    /// Open the Repository Browser, loading the tracked file list in the background.
    pub fn open_repo_browse(&mut self) {
        if self.browse_state.is_some() {
            self.state = AppState::RepoBrowseTree;
            return;
        }

        let return_state = self.state;
        let repo_root = self.browse_repo_root();

        let mut state = BrowseState::new(repo_root.clone(), return_state);

        let (tx, rx) = mpsc::channel(1);
        state.paths_receiver = Some(rx);
        tokio::task::spawn_blocking(move || {
            let _ = tx.blocking_send(list_repository_files(&repo_root));
        });

        self.browse_state = Some(state);
        self.state = AppState::RepoBrowseTree;
    }

    /// Working directory the browser walks — `--working-dir` when given, else the process cwd.
    fn browse_repo_root(&self) -> PathBuf {
        self.working_dir
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// Leave the browser, returning to whichever screen opened it.
    pub fn close_repo_browse(&mut self) {
        let return_state = self
            .browse_state
            .as_ref()
            .map(|state| state.return_state)
            .unwrap_or(AppState::FileList);
        self.browse_state = None;
        self.state = return_state;
    }

    /// Drain background browse channels: file list, symbol index, highlighting.
    pub(crate) fn poll_browse_updates(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };

        let mut paths_ready = false;
        if let Some(rx) = state.paths_receiver.as_mut() {
            match rx.try_recv() {
                Ok(Ok(paths)) => {
                    state.paths_receiver = None;
                    state.set_paths(paths);
                    paths_ready = true;
                }
                Ok(Err(message)) => {
                    state.paths_receiver = None;
                    state.paths = LoadState::Error(message);
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.paths_receiver = None;
                    state.paths = LoadState::Error("file listing task ended".to_string());
                }
            }
        }

        if let Some(rx) = state.index_receiver.as_mut() {
            match rx.try_recv() {
                Ok(index) => {
                    state.index_receiver = None;
                    state.index = IndexState::Ready(Arc::new(index));
                    state.refresh_open_file_symbols();
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.index_receiver = None;
                    state.index = IndexState::Failed;
                }
            }
        }

        if let Some(rx) = state.highlight_receiver.as_mut() {
            match rx.try_recv() {
                Ok((path, cache)) => {
                    state.highlight_receiver = None;
                    state.apply_highlighted_cache(&path, cache);
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    state.highlight_receiver = None;
                }
            }
        }

        if paths_ready {
            self.start_symbol_index_build();
        }
    }

    /// Kick off the repository-wide symbol index on a background thread.
    fn start_symbol_index_build(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let LoadState::Loaded(ref paths) = state.paths else {
            return;
        };
        if matches!(state.index, IndexState::Building) {
            return;
        }

        let paths = paths.clone();
        let repo_root = state.repo_root.clone();
        let (tx, rx) = mpsc::channel(1);
        state.index = IndexState::Building;
        state.index_receiver = Some(rx);

        tokio::task::spawn_blocking(move || {
            let index = SymbolIndex::build(&repo_root, &paths);
            let _ = tx.blocking_send(index);
        });
    }

    /// Open the file currently selected in the tree.
    pub(crate) fn browse_open_selected(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let Some(path) = state.selected_path().map(str::to_string) else {
            return;
        };
        self.browse_open_path(&path, 0);
    }

    /// Open `path` (repository-relative) and place the cursor on `line` (0-based).
    pub(crate) fn browse_open_path(&mut self, path: &str, line: usize) {
        let tab_width = self.config.diff.tab_width;
        let theme = self.config.diff.theme.clone();

        let Some(state) = self.browse_state.as_mut() else {
            return;
        };

        if state.open.as_ref().is_some_and(|open| open.path == path) {
            state.focus_line(line);
            self.state = AppState::RepoBrowseFile;
            return;
        }

        let absolute = state.repo_root.join(path);
        let open = match load_file(&absolute, path, tab_width) {
            Ok(open) => open,
            Err(message) => {
                state.open = None;
                state.status = Some(message);
                return;
            }
        };

        state.status = None;
        state.open = Some(open);
        state.focus_line(line);
        state.sync_tree_to_open_file();
        state.refresh_open_file_symbols();

        // Highlighting is the expensive half; the plain cache is already on
        // screen, so upgrade it in the background rather than stalling a
        // keypress on a 20,000-line file.
        if let Some(ref open) = state.open {
            if open.viewable {
                let (tx, rx) = mpsc::channel(1);
                state.highlight_receiver = Some(rx);
                let path = open.path.clone();
                let patch = open.patch.clone();
                tokio::task::spawn_blocking(move || {
                    let mut pool = ParserPool::new();
                    let cache = crate::ui::diff_view::build_diff_cache(
                        &patch, &path, &theme, &mut pool, true, tab_width,
                    );
                    let _ = tx.blocking_send((path, cache));
                });
            }
        }

        self.state = AppState::RepoBrowseFile;
    }

    /// Jump to the definition of the identifier under the cursor.
    ///
    /// Returns `false` when there is no index yet, no identifier under the
    /// cursor, or no definition for it — the caller surfaces the reason.
    pub(crate) fn browse_go_to_definition(&mut self) -> bool {
        let Some(state) = self.browse_state.as_ref() else {
            return false;
        };
        let IndexState::Ready(ref index) = state.index else {
            return false;
        };
        let Some(open) = state.open.as_ref() else {
            return false;
        };
        let Some(line) = open.source_line(state.cursor_line) else {
            return false;
        };

        let identifiers = crate::symbol::extract_all_identifiers(line);
        if identifiers.is_empty() {
            return false;
        }

        // Prefer a definition for the *first* identifier that resolves, which
        // for `foo.bar()` and `let x = Config::new()` alike is the reading
        // order a human would try.
        let index = Arc::clone(index);
        for (name, _, _) in identifiers {
            let hits = index.definitions(&name);
            let Some(hit) = hits.first() else {
                continue;
            };
            let path = hit.path.to_string();
            let target_line = hit.symbol.line.saturating_sub(1);
            self.browse_push_jump();
            self.browse_open_path(&path, target_line);
            return true;
        }

        false
    }

    /// Record the current position so `jump_back` can return to it.
    pub(crate) fn browse_push_jump(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let Some(open) = state.open.as_ref() else {
            return;
        };
        let entry = BrowseJump {
            path: open.path.clone(),
            line: state.cursor_line,
            scroll: state.scroll_offset,
        };
        state.jump_stack.push(entry);
        // Same bound as the diff view's jump stack.
        if state.jump_stack.len() > 100 {
            state.jump_stack.remove(0);
        }
    }

    /// Return to the position recorded by the most recent jump.
    pub(crate) fn browse_jump_back(&mut self) -> bool {
        let Some(state) = self.browse_state.as_mut() else {
            return false;
        };
        let Some(entry) = state.jump_stack.pop() else {
            return false;
        };
        self.browse_open_path(&entry.path, entry.line);
        if let Some(state) = self.browse_state.as_mut() {
            state.scroll_offset = entry.scroll;
        }
        true
    }
}

/// A jump-stack entry inside the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseJump {
    pub path: String,
    pub line: usize,
    pub scroll: usize,
}

/// Lifecycle of the repository symbol index.
#[derive(Debug, Default)]
pub enum IndexState {
    #[default]
    Idle,
    Building,
    Ready(Arc<SymbolIndex>),
    /// The background task died; symbol features degrade but browsing continues.
    Failed,
}

impl IndexState {
    pub fn ready(&self) -> Option<&SymbolIndex> {
        match self {
            Self::Ready(index) => Some(index),
            _ => None,
        }
    }

    /// Short status text for the header.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "symbols: -",
            Self::Building => "symbols: indexing",
            Self::Ready(_) => "symbols: ready",
            Self::Failed => "symbols: unavailable",
        }
    }
}

/// A file opened in the browser.
pub struct OpenFile {
    /// Repository-relative path.
    pub path: String,
    /// The file rendered as an all-context pseudo-patch, so the diff
    /// highlighter can be reused verbatim.
    pub patch: String,
    pub cache: DiffCache,
    /// Source lines, retained for identifier extraction under the cursor.
    pub lines: Vec<String>,
    pub symbols: Vec<Symbol>,
    /// False for binary or oversized files, which render as a notice.
    pub viewable: bool,
    /// Why the file is not viewable, when it is not.
    pub notice: Option<String>,
}

impl std::fmt::Debug for OpenFile {
    /// `DiffCache` holds a string interner and is not `Debug`; the fields that
    /// matter for diagnosing a failure are the path, size and viewability.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenFile")
            .field("path", &self.path)
            .field("lines", &self.lines.len())
            .field("symbols", &self.symbols.len())
            .field("viewable", &self.viewable)
            .field("notice", &self.notice)
            .finish()
    }
}

impl OpenFile {
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Source text of a 0-based line.
    pub fn source_line(&self, line: usize) -> Option<&str> {
        self.lines.get(line).map(String::as_str)
    }
}

/// Which overlay is on top of the browser, if any.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum BrowseOverlay {
    #[default]
    None,
    /// Outline of the open file.
    Outline { selected: usize },
    /// Repository-wide fuzzy symbol search.
    SymbolSearch { query: String, selected: usize },
}

/// Everything the Repository Browser needs. `None` on [`App`] means inactive.
pub struct BrowseState {
    pub repo_root: PathBuf,
    pub paths: LoadState<Vec<String>>,
    pub tree: crate::app::file_tree::FileTreeState,
    pub filter: Option<ListFilter>,
    pub open: Option<OpenFile>,
    /// 0-based cursor line within the open file.
    pub cursor_line: usize,
    pub scroll_offset: usize,
    pub index: IndexState,
    pub overlay: BrowseOverlay,
    pub jump_stack: Vec<BrowseJump>,
    /// Transient message shown in the footer (unreadable file, no definition, …).
    pub status: Option<String>,
    pub return_state: AppState,
    pub(crate) paths_receiver: Option<mpsc::Receiver<Result<Vec<String>, String>>>,
    pub(crate) index_receiver: Option<mpsc::Receiver<SymbolIndex>>,
    pub(crate) highlight_receiver: Option<mpsc::Receiver<(String, DiffCache)>>,
}

impl BrowseState {
    pub fn new(repo_root: PathBuf, return_state: AppState) -> Self {
        Self {
            repo_root,
            paths: LoadState::Loading,
            tree: crate::app::file_tree::FileTreeState::new(),
            filter: None,
            open: None,
            cursor_line: 0,
            scroll_offset: 0,
            index: IndexState::Idle,
            overlay: BrowseOverlay::None,
            jump_stack: Vec::new(),
            status: None,
            return_state,
            paths_receiver: None,
            index_receiver: None,
            highlight_receiver: None,
        }
    }

    /// Install a freshly listed set of tracked paths and rebuild the tree.
    pub fn set_paths(&mut self, paths: Vec<String>) {
        self.paths = LoadState::Loaded(paths);
        self.rebuild_tree();
    }

    /// All paths currently listed, before filtering.
    pub fn all_paths(&self) -> &[String] {
        match self.paths {
            LoadState::Loaded(ref paths) => paths,
            _ => &[],
        }
    }

    /// Rebuild the tree from the paths that survive the active filter.
    pub fn rebuild_tree(&mut self) {
        let visible: Vec<(usize, String)> = match self.filter {
            Some(ref filter) => filter
                .matched_indices
                .iter()
                .filter_map(|index| {
                    self.all_paths()
                        .get(*index)
                        .map(|path| (*index, path.clone()))
                })
                .collect(),
            None => self
                .all_paths()
                .iter()
                .enumerate()
                .map(|(index, path)| (index, path.clone()))
                .collect(),
        };
        self.tree.rebuild_owned(visible);
    }

    /// Re-apply the filter query to the path list.
    pub fn apply_filter(&mut self) {
        let paths: Vec<String> = self.all_paths().to_vec();
        if let Some(filter) = self.filter.as_mut() {
            filter.apply(&paths, |path: &String, query| {
                path.to_lowercase().contains(query)
            });
            filter.sync_selection();
        }
        self.rebuild_tree();
    }

    /// Repository-relative path under the tree cursor, if it is a file row.
    pub fn selected_path(&self) -> Option<&str> {
        let index = self.tree.selected_file_index()?;
        self.all_paths().get(index).map(String::as_str)
    }

    /// Move the tree cursor onto the open file so the panes agree.
    pub fn sync_tree_to_open_file(&mut self) {
        let Some(ref open) = self.open else {
            return;
        };
        let Some(index) = self.all_paths().iter().position(|path| *path == open.path) else {
            return;
        };
        if let Some(row) = self.tree.find_row_for_file(index) {
            self.tree.selected_row = row;
        }
    }

    /// Place the cursor on a 0-based line, clamped to the file.
    pub fn focus_line(&mut self, line: usize) {
        let last = self
            .open
            .as_ref()
            .map(|open| open.line_count().saturating_sub(1))
            .unwrap_or(0);
        self.cursor_line = line.min(last);
        // Keep the target roughly a third down the viewport rather than glued
        // to the top edge, which reads better when jumping into a definition.
        self.scroll_offset = self.cursor_line.saturating_sub(8);
    }

    /// Scroll so the cursor stays inside a viewport `height` rows tall.
    pub fn clamp_scroll(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.cursor_line < self.scroll_offset {
            self.scroll_offset = self.cursor_line;
        } else if self.cursor_line >= self.scroll_offset + height {
            self.scroll_offset = self.cursor_line + 1 - height;
        }
    }

    /// Move the cursor by `delta` lines, clamped to the file.
    pub fn move_cursor(&mut self, delta: isize) {
        let last = self
            .open
            .as_ref()
            .map(|open| open.line_count().saturating_sub(1))
            .unwrap_or(0);
        let next = self.cursor_line as isize + delta;
        self.cursor_line = next.clamp(0, last as isize) as usize;
    }

    /// Attach symbols for the open file, preferring the index when it is ready.
    pub fn refresh_open_file_symbols(&mut self) {
        let indexed = self.index.ready().and_then(|index| {
            self.open
                .as_ref()
                .and_then(|open| index.file_symbols(&open.path))
                .map(<[Symbol]>::to_vec)
        });
        if let (Some(open), Some(symbols)) = (self.open.as_mut(), indexed) {
            open.symbols = symbols;
        }
    }

    /// Swap in the fully highlighted cache once the background task delivers it.
    pub fn apply_highlighted_cache(&mut self, path: &str, cache: DiffCache) {
        if let Some(open) = self.open.as_mut() {
            if open.path == path {
                open.cache = cache;
            }
        }
    }

    /// Symbol rows for the outline overlay.
    pub fn outline_symbols(&self) -> &[Symbol] {
        self.open
            .as_ref()
            .map(|open| open.symbols.as_slice())
            .unwrap_or(&[])
    }

    /// Current symbol-search results as `(path, line, label)` rows.
    pub fn symbol_search_results(&self, query: &str) -> Vec<(String, usize, String)> {
        let Some(index) = self.index.ready() else {
            return Vec::new();
        };
        index
            .search(query, MAX_SYMBOL_SEARCH_RESULTS)
            .into_iter()
            .map(|hit| {
                (
                    hit.path.to_string(),
                    hit.symbol.line,
                    format!(
                        "{} {}  {}:{}",
                        hit.symbol.kind.glyph(),
                        hit.symbol.name,
                        hit.path,
                        hit.symbol.line
                    ),
                )
            })
            .collect()
    }
}

/// List the repository's files: tracked plus untracked-but-not-ignored.
///
/// `git ls-files` is used rather than a filesystem walk so `.gitignore`,
/// submodules and sparse checkouts are all honoured for free — the same
/// contract every other octorus screen already relies on.
///
/// `--others` matters more than it looks: a file an agent wrote thirty seconds
/// ago is not committed yet, and a repository viewer that cannot show it is
/// blind exactly when the user most needs to read.
pub fn list_repository_files(repo_root: &std::path::Path) -> Result<Vec<String>, String> {
    let output = std::process::Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("failed to run git ls-files: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(if stderr.is_empty() {
            "not a git repository".to_string()
        } else {
            stderr.to_string()
        });
    }

    Ok(parse_ls_files(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse `git ls-files` output into a sorted, de-duplicated path list.
pub fn parse_ls_files(stdout: &str) -> Vec<String> {
    let mut paths: Vec<String> = stdout
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    paths.sort();
    paths.dedup();
    paths.truncate(MAX_BROWSE_FILES);
    paths
}

/// Read a file and prepare it for display.
fn load_file(absolute: &std::path::Path, path: &str, tab_width: u8) -> Result<OpenFile, String> {
    let metadata = std::fs::metadata(absolute).map_err(|e| format!("{path}: {e}"))?;
    if !metadata.is_file() {
        return Err(format!("{path}: not a regular file"));
    }

    if metadata.len() > MAX_VIEWABLE_FILE_BYTES {
        return Ok(unviewable(
            path,
            format!(
                "File is {} — too large to display ({} limit).",
                human_bytes(metadata.len()),
                human_bytes(MAX_VIEWABLE_FILE_BYTES)
            ),
            tab_width,
        ));
    }

    let bytes = std::fs::read(absolute).map_err(|e| format!("{path}: {e}"))?;
    let Ok(source) = String::from_utf8(bytes) else {
        return Ok(unviewable(
            path,
            "Binary file — no text preview.".to_string(),
            tab_width,
        ));
    };

    let lines: Vec<String> = source.lines().map(str::to_string).collect();
    let patch = build_file_patch(&source);
    let cache = crate::ui::diff_view::build_plain_diff_cache(&patch, tab_width);

    Ok(OpenFile {
        path: path.to_string(),
        patch,
        cache,
        lines,
        symbols: Vec::new(),
        viewable: true,
        notice: None,
    })
}

fn unviewable(path: &str, notice: String, tab_width: u8) -> OpenFile {
    let patch = build_file_patch("");
    OpenFile {
        path: path.to_string(),
        cache: crate::ui::diff_view::build_plain_diff_cache(&patch, tab_width),
        patch,
        lines: Vec::new(),
        symbols: Vec::new(),
        viewable: false,
        notice: Some(notice),
    }
}

/// Render file content as an all-context pseudo-patch.
///
/// This is what lets the browser reuse the diff highlighter — and therefore
/// every language, theme and injection the diff view already supports —
/// without a second rendering path to keep in sync.
pub fn build_file_patch(source: &str) -> String {
    let source = source.replace("\r\n", "\n");
    let line_count = source.lines().count().max(1);
    let mut patch = String::with_capacity(source.len() + 32);
    patch.push_str(&format!("@@ -1,{line_count} +1,{line_count} @@\n"));
    for line in source.lines() {
        patch.push(' ');
        patch.push_str(line);
        patch.push('\n');
    }
    patch
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::{FileSymbols, SymbolKind};

    fn state_with_paths(paths: &[&str]) -> BrowseState {
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        state.set_paths(paths.iter().map(|p| p.to_string()).collect());
        state
    }

    // ===== git ls-files parsing =====

    #[test]
    fn test_parse_ls_files_sorts_and_dedups() {
        let out = "src/b.rs\nsrc/a.rs\n\nsrc/a.rs\nREADME.md\n";
        assert_eq!(
            parse_ls_files(out),
            vec!["README.md", "src/a.rs", "src/b.rs"]
        );
    }

    #[test]
    fn test_parse_ls_files_empty_repository() {
        assert!(parse_ls_files("").is_empty());
        assert!(parse_ls_files("\n\n").is_empty());
    }

    #[test]
    fn test_parse_ls_files_handles_crlf_output() {
        assert_eq!(parse_ls_files("src/a.rs\r\n"), vec!["src/a.rs"]);
    }

    #[test]
    fn test_parse_ls_files_keeps_paths_with_spaces() {
        assert_eq!(
            parse_ls_files("docs/my notes.md\n"),
            vec!["docs/my notes.md"]
        );
    }

    // ===== pseudo-patch conversion =====

    #[test]
    fn test_build_file_patch() {
        assert_eq!(build_file_patch("a\nb"), "@@ -1,2 +1,2 @@\n a\n b\n");
    }

    #[test]
    fn test_build_file_patch_empty_file() {
        assert_eq!(build_file_patch(""), "@@ -1,1 +1,1 @@\n");
    }

    #[test]
    fn test_build_file_patch_preserves_diff_looking_lines() {
        // A source line starting with '-' must not be read back as a deletion.
        assert_eq!(
            build_file_patch("-not a deletion\n+not an addition"),
            "@@ -1,2 +1,2 @@\n -not a deletion\n +not an addition\n"
        );
    }

    #[test]
    fn test_build_file_patch_normalises_crlf() {
        assert_eq!(build_file_patch("a\r\nb\r\n"), "@@ -1,2 +1,2 @@\n a\n b\n");
    }

    // ===== tree + filter =====

    #[test]
    fn test_tree_built_from_paths() {
        let state = state_with_paths(&["src/app.rs", "src/ui/mod.rs", "README.md"]);
        insta::assert_snapshot!(state.tree.dump_tree(), @r"
        ▼ src/
          ▼ ui/
            mod.rs
          app.rs
        README.md
        ");
    }

    #[test]
    fn test_empty_repository_yields_empty_tree() {
        let state = state_with_paths(&[]);
        assert_eq!(state.tree.row_count(), 0);
        assert!(state.selected_path().is_none());
    }

    #[test]
    fn test_filter_narrows_tree_and_keeps_source_indices() {
        let mut state = state_with_paths(&["src/app.rs", "src/ui.rs", "README.md"]);
        let mut filter = ListFilter::new();
        filter.query = "ui".to_string();
        state.filter = Some(filter);
        state.apply_filter();

        assert_eq!(state.tree.dump_tree(), "▼ src/\n  ui.rs");
        // Selecting the only match must resolve to the original path, not to
        // index 0 of the filtered list.
        state.tree.selected_row = 1;
        assert_eq!(state.selected_path(), Some("src/ui.rs"));
    }

    #[test]
    fn test_filter_with_no_matches_leaves_nothing_selectable() {
        let mut state = state_with_paths(&["src/app.rs"]);
        let mut filter = ListFilter::new();
        filter.query = "zzz".to_string();
        state.filter = Some(filter);
        state.apply_filter();

        assert_eq!(state.tree.row_count(), 0);
        assert!(state.selected_path().is_none());
    }

    // ===== cursor and scrolling =====

    fn state_with_open_file(line_count: usize) -> BrowseState {
        let mut state = state_with_paths(&["src/a.rs"]);
        let source: String = (0..line_count)
            .map(|i| format!("line {i}\n"))
            .collect::<String>();
        let patch = build_file_patch(&source);
        state.open = Some(OpenFile {
            path: "src/a.rs".to_string(),
            cache: crate::ui::diff_view::build_plain_diff_cache(&patch, 4),
            patch,
            lines: source.lines().map(str::to_string).collect(),
            symbols: Vec::new(),
            viewable: true,
            notice: None,
        });
        state
    }

    #[test]
    fn test_move_cursor_clamps_to_file_bounds() {
        let mut state = state_with_open_file(5);
        state.move_cursor(100);
        assert_eq!(state.cursor_line, 4);
        state.move_cursor(-100);
        assert_eq!(state.cursor_line, 0);
    }

    #[test]
    fn test_move_cursor_without_open_file_stays_at_zero() {
        let mut state = state_with_paths(&["src/a.rs"]);
        state.move_cursor(10);
        assert_eq!(state.cursor_line, 0);
    }

    #[test]
    fn test_clamp_scroll_follows_cursor_down_and_up() {
        let mut state = state_with_open_file(100);
        state.cursor_line = 40;
        state.scroll_offset = 0;
        state.clamp_scroll(10);
        assert_eq!(state.scroll_offset, 31);

        state.cursor_line = 5;
        state.clamp_scroll(10);
        assert_eq!(state.scroll_offset, 5);
    }

    #[test]
    fn test_clamp_scroll_with_zero_height_is_a_noop() {
        let mut state = state_with_open_file(10);
        state.scroll_offset = 3;
        state.clamp_scroll(0);
        assert_eq!(state.scroll_offset, 3);
    }

    #[test]
    fn test_focus_line_clamps_and_offsets_scroll() {
        let mut state = state_with_open_file(50);
        state.focus_line(30);
        assert_eq!(state.cursor_line, 30);
        assert_eq!(state.scroll_offset, 22);

        state.focus_line(999);
        assert_eq!(state.cursor_line, 49);
    }

    #[test]
    fn test_focus_line_near_top_does_not_underflow() {
        let mut state = state_with_open_file(50);
        state.focus_line(2);
        assert_eq!(state.scroll_offset, 0);
    }

    // ===== symbols =====

    fn ready_index() -> IndexState {
        IndexState::Ready(Arc::new(SymbolIndex::from_files(vec![FileSymbols {
            path: "src/a.rs".to_string(),
            symbols: vec![Symbol {
                name: "alpha".to_string(),
                kind: SymbolKind::Function,
                line: 3,
                column: 3,
                depth: 0,
            }],
        }])))
    }

    #[test]
    fn test_refresh_open_file_symbols_from_index() {
        let mut state = state_with_open_file(10);
        assert!(state.outline_symbols().is_empty());

        state.index = ready_index();
        state.refresh_open_file_symbols();

        assert_eq!(state.outline_symbols().len(), 1);
        assert_eq!(state.outline_symbols()[0].name, "alpha");
    }

    #[test]
    fn test_symbol_search_results_before_index_is_ready() {
        let state = state_with_open_file(10);
        assert!(state.symbol_search_results("alpha").is_empty());
    }

    #[test]
    fn test_symbol_search_results_render_location() {
        let mut state = state_with_open_file(10);
        state.index = ready_index();
        assert_eq!(
            state.symbol_search_results("alpha"),
            vec![("src/a.rs".to_string(), 3, "ƒ alpha  src/a.rs:3".to_string())]
        );
    }

    #[test]
    fn test_index_state_labels() {
        assert_eq!(IndexState::Idle.label(), "symbols: -");
        assert_eq!(IndexState::Building.label(), "symbols: indexing");
        assert_eq!(ready_index().label(), "symbols: ready");
        assert_eq!(IndexState::Failed.label(), "symbols: unavailable");
        assert!(IndexState::Building.ready().is_none());
    }

    // ===== file loading =====

    #[test]
    fn test_load_file_reads_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        let open = load_file(&dir.path().join("a.rs"), "a.rs", 4).unwrap();
        assert!(open.viewable);
        assert_eq!(open.lines, vec!["fn main() {}"]);
        assert_eq!(open.source_line(0), Some("fn main() {}"));
        assert_eq!(open.source_line(9), None);
    }

    #[test]
    fn test_load_file_empty_file_is_viewable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.rs"), "").unwrap();
        let open = load_file(&dir.path().join("empty.rs"), "empty.rs", 4).unwrap();
        assert!(open.viewable);
        assert_eq!(open.line_count(), 0);
    }

    #[test]
    fn test_load_file_binary_is_reported_not_rendered() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let open = load_file(&dir.path().join("blob.bin"), "blob.bin", 4).unwrap();
        assert!(!open.viewable);
        assert_eq!(
            open.notice.as_deref(),
            Some("Binary file — no text preview.")
        );
    }

    #[test]
    fn test_load_file_missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_file(&dir.path().join("nope.rs"), "nope.rs", 4).unwrap_err();
        assert!(err.starts_with("nope.rs:"), "{err}");
    }

    #[test]
    fn test_load_file_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let err = load_file(&dir.path().join("sub"), "sub", 4).unwrap_err();
        assert_eq!(err, "sub: not a regular file");
    }

    #[test]
    fn test_human_bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(8 * 1024 * 1024), "8.0 MiB");
    }

    // ===== listing a real repository =====

    #[test]
    fn test_list_repository_files_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        // A bare temp dir is not a git repo — the browser must report that
        // rather than showing an empty tree as if the repo had no files.
        assert!(list_repository_files(dir.path()).is_err());
    }

    #[test]
    fn test_list_repository_files_includes_untracked_but_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git")
        };
        if !git(&["init"]).status.success() {
            return; // git unavailable in this environment
        }
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("committed.rs"), "pub fn a() {}\n").unwrap();
        git(&["add", "committed.rs"]);
        std::fs::write(dir.path().join("brand_new.rs"), "pub fn b() {}\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "pub fn c() {}\n").unwrap();

        let paths = list_repository_files(dir.path()).unwrap();
        assert!(paths.contains(&"committed.rs".to_string()), "{paths:?}");
        assert!(
            paths.contains(&"brand_new.rs".to_string()),
            "a file written seconds ago must be browsable: {paths:?}"
        );
        assert!(!paths.contains(&"ignored.rs".to_string()), "{paths:?}");
    }
}
