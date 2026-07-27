//! Key handling for the Repository Browser.
//!
//! Two focus states (tree / file) plus two overlays (outline / symbol search).
//! Overlays are checked first so they behave as modal layers rather than as an
//! extra set of conditionals sprinkled through the pane handlers.

use anyhow::Result;
use crossterm::event::{self, KeyCode, KeyModifiers};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::Stdout;

use crate::filter::ListFilter;
use crate::keybinding::{event_to_keybinding, SequenceMatch};

use super::browse::{BrowseOverlay, IndexState};
use super::{App, AppState};

/// Rows moved by a page key when the real viewport height is unknown.
const PAGE_STEP: usize = 15;

impl App {
    pub(crate) fn handle_repo_browse_tree_input(&mut self, key: event::KeyEvent) -> Result<()> {
        if self.handle_browse_overlay_input(key) {
            return Ok(());
        }
        if self.handle_browse_filter_input(&key) {
            return Ok(());
        }
        if self.handle_browse_shared_input(&key) {
            return Ok(());
        }
        if self.handle_browse_tree_sequence_input(&key) {
            return Ok(());
        }

        let kb = self.config.keybindings.clone();

        if self.matches_single_key(&key, &kb.open_panel)
            || self.matches_single_key(&key, &kb.move_right)
        {
            let is_dir = self
                .browse_state
                .as_ref()
                .is_some_and(|state| state.tree.selected_dir_path().is_some());
            if is_dir {
                if let Some(state) = self.browse_state.as_mut() {
                    state.tree.toggle_expand();
                }
            } else {
                self.browse_open_selected();
            }
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.quit) {
            self.close_repo_browse();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.filter) {
            if let Some(state) = self.browse_state.as_mut() {
                state.filter = Some(ListFilter::new());
                state.apply_filter();
            }
            return Ok(());
        }

        let move_down = self.matches_single_key(&key, &kb.move_down);
        let move_up = self.matches_single_key(&key, &kb.move_up);
        let page_down = self.matches_single_key(&key, &kb.page_down);
        let page_up = self.matches_single_key(&key, &kb.page_up);
        let jump_last = self.matches_single_key(&key, &kb.jump_to_last);

        let Some(state) = self.browse_state.as_mut() else {
            return Ok(());
        };

        if move_down {
            state.tree.move_down();
        } else if move_up {
            state.tree.move_up();
        } else if page_down {
            state.tree.page_down(PAGE_STEP);
        } else if page_up {
            state.tree.page_up(PAGE_STEP);
        } else if jump_last {
            state.tree.jump_to_last();
        }

        Ok(())
    }

    pub(crate) fn handle_repo_browse_file_input(
        &mut self,
        key: event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        if self.handle_browse_overlay_input(key) {
            return Ok(());
        }
        if self.handle_browse_shared_input(&key) {
            return Ok(());
        }
        if self.handle_browse_sequence_input(&key, terminal)? {
            return Ok(());
        }

        let kb = self.config.keybindings.clone();

        if self.matches_single_key(&key, &kb.symbol_outline) {
            self.open_browse_outline();
            return Ok(());
        }

        if self.matches_single_key(&key, &kb.move_left) || self.matches_single_key(&key, &kb.quit) {
            self.state = AppState::RepoBrowseTree;
            return Ok(());
        }

        let page_down =
            self.matches_single_key(&key, &kb.page_down) || key.code == KeyCode::PageDown;
        let page_up = self.matches_single_key(&key, &kb.page_up) || key.code == KeyCode::PageUp;
        let move_down = self.matches_single_key(&key, &kb.move_down);
        let move_up = self.matches_single_key(&key, &kb.move_up);
        let jump_last = self.matches_single_key(&key, &kb.jump_to_last);

        let Some(state) = self.browse_state.as_mut() else {
            return Ok(());
        };

        if move_down {
            state.move_cursor(1);
        } else if move_up {
            state.move_cursor(-1);
        } else if page_down {
            state.move_cursor(PAGE_STEP as isize);
        } else if page_up {
            state.move_cursor(-(PAGE_STEP as isize));
        } else if jump_last {
            let last = state
                .open
                .as_ref()
                .map(|open| open.line_count().saturating_sub(1))
                .unwrap_or(0);
            state.focus_line(last);
        }

        Ok(())
    }

    /// Resolve tree-pane sequences (`Space /` filter, `gg` jump to first).
    ///
    /// The filter binding is a two-key sequence by default, so it cannot be
    /// matched with `matches_single_key` the way the other tree keys are.
    fn handle_browse_tree_sequence_input(&mut self, key: &event::KeyEvent) -> bool {
        self.check_sequence_timeout();

        let kb = self.config.keybindings.clone();
        let Some(binding) = event_to_keybinding(key) else {
            return false;
        };

        if !self.pending_keys.is_empty() {
            self.push_pending_key(binding);

            if self.try_match_sequence(&kb.filter) == SequenceMatch::Full {
                self.clear_pending_keys();
                if let Some(state) = self.browse_state.as_mut() {
                    state.filter = Some(ListFilter::new());
                    state.apply_filter();
                }
                return true;
            }
            if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                self.clear_pending_keys();
                if let Some(state) = self.browse_state.as_mut() {
                    state.tree.jump_to_first();
                }
                return true;
            }

            self.clear_pending_keys();
            return false;
        }

        let starts_sequence = self.key_could_match_sequence(key, &kb.filter)
            || self.key_could_match_sequence(key, &kb.jump_to_first);
        if starts_sequence {
            self.push_pending_key(binding);
            return true;
        }

        false
    }

    /// Keys shared by both panes. Returns `true` when the key was consumed.
    fn handle_browse_shared_input(&mut self, key: &event::KeyEvent) -> bool {
        let kb = self.config.keybindings.clone();

        if self.matches_single_key(key, &kb.symbol_search) {
            self.open_browse_symbol_search();
            return true;
        }
        if self.matches_single_key(key, &kb.help) {
            self.previous_state = self.state;
            self.state = AppState::Help;
            return true;
        }
        if self.matches_single_key(key, &kb.toggle_zen_mode) {
            self.toggle_zen_mode();
            return true;
        }
        if self.matches_single_key(key, &kb.jump_back) {
            if !self.browse_jump_back() {
                self.set_browse_status("No position to jump back to");
            }
            return true;
        }
        false
    }

    /// Resolve `gg` / `gb` / `gd` / `gf`. Returns `true` when the key was consumed.
    fn handle_browse_sequence_input(
        &mut self,
        key: &event::KeyEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<bool> {
        self.check_sequence_timeout();

        let kb = self.config.keybindings.clone();
        let Some(binding) = event_to_keybinding(key) else {
            return Ok(false);
        };

        if !self.pending_keys.is_empty() {
            self.push_pending_key(binding);

            if self.try_match_sequence(&kb.toggle_blame) == SequenceMatch::Full {
                self.clear_pending_keys();
                self.toggle_browse_blame();
                return Ok(true);
            }
            if self.try_match_sequence(&kb.go_to_definition) == SequenceMatch::Full {
                self.clear_pending_keys();
                self.browse_run_go_to_definition();
                return Ok(true);
            }
            if self.try_match_sequence(&kb.go_to_file) == SequenceMatch::Full {
                self.clear_pending_keys();
                self.open_browse_file_in_editor(terminal)?;
                return Ok(true);
            }
            if self.try_match_sequence(&kb.jump_to_first) == SequenceMatch::Full {
                self.clear_pending_keys();
                if let Some(state) = self.browse_state.as_mut() {
                    state.focus_line(0);
                }
                return Ok(true);
            }

            self.clear_pending_keys();
            return Ok(false);
        }

        let starts_sequence = self.key_could_match_sequence(key, &kb.toggle_blame)
            || self.key_could_match_sequence(key, &kb.go_to_definition)
            || self.key_could_match_sequence(key, &kb.go_to_file)
            || self.key_could_match_sequence(key, &kb.jump_to_first);
        if starts_sequence {
            self.push_pending_key(binding);
            return Ok(true);
        }

        Ok(false)
    }

    /// Jump to the definition under the cursor, reporting why when it cannot.
    fn browse_run_go_to_definition(&mut self) {
        // Checked before the index, because a pending load means we have not
        // read the cursor's line yet — there is no identifier to resolve, and
        // "No definition found" would be a claim about a file we have not seen.
        let pending = self
            .browse_state
            .as_ref()
            .is_some_and(|state| state.open_is_pending());
        if pending {
            self.set_browse_status("Still opening this file");
            return;
        }
        let indexing = self
            .browse_state
            .as_ref()
            .is_some_and(|state| state.index.ready().is_none());
        if indexing {
            self.set_browse_status("Symbol index is still building");
            return;
        }
        if !self.browse_go_to_definition() {
            self.set_browse_status("No definition found");
        }
    }

    /// Hand the open file to the configured external editor at the cursor line.
    fn open_browse_file_in_editor(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let Some(state) = self.browse_state.as_ref() else {
            return Ok(());
        };
        let Some(ref open) = state.open else {
            return Ok(());
        };
        let path = state.repo_root.join(&open.path);
        let line = state.cursor_line + 1;
        let editor = self.config.editor.clone();

        crate::ui::restore_terminal(terminal)?;
        let result =
            crate::editor::open_file_at_line(editor.as_deref(), &path.to_string_lossy(), line);
        *terminal = crate::ui::setup_terminal()?;
        terminal.clear()?;

        if let Err(e) = result {
            self.set_browse_status(&format!("Editor failed: {e}"));
        }
        Ok(())
    }

    /// Handle keys for the tree's keyword filter. Returns `true` when consumed.
    fn handle_browse_filter_input(&mut self, key: &event::KeyEvent) -> bool {
        let Some(state) = self.browse_state.as_mut() else {
            return false;
        };
        if !state
            .filter
            .as_ref()
            .is_some_and(|filter| filter.input_active)
        {
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                state.filter = None;
                state.rebuild_tree();
            }
            KeyCode::Enter => {
                if let Some(filter) = state.filter.as_mut() {
                    filter.input_active = false;
                }
            }
            KeyCode::Backspace => {
                if let Some(filter) = state.filter.as_mut() {
                    filter.delete_char();
                }
                state.apply_filter();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(filter) = state.filter.as_mut() {
                    filter.clear_query();
                }
                state.apply_filter();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(filter) = state.filter.as_mut() {
                    filter.insert_char(c);
                }
                state.apply_filter();
            }
            _ => return false,
        }
        true
    }

    /// Dispatch to whichever overlay is open. Returns `true` when consumed.
    fn handle_browse_overlay_input(&mut self, key: event::KeyEvent) -> bool {
        let Some(state) = self.browse_state.as_ref() else {
            return false;
        };

        match state.overlay {
            BrowseOverlay::None => false,
            BrowseOverlay::Outline { .. } => {
                self.handle_browse_outline_input(key);
                true
            }
            BrowseOverlay::SymbolSearch { .. } => {
                self.handle_browse_symbol_search_input(key);
                true
            }
        }
    }

    fn handle_browse_outline_input(&mut self, key: event::KeyEvent) {
        let kb = self.config.keybindings.clone();
        let close = key.code == KeyCode::Esc || self.matches_single_key(&key, &kb.quit);
        let down = self.matches_single_key(&key, &kb.move_down);
        let up = self.matches_single_key(&key, &kb.move_up);
        let confirm = self.matches_single_key(&key, &kb.open_panel);

        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let count = state.outline_symbols().len();

        if close {
            state.overlay = BrowseOverlay::None;
            return;
        }

        let BrowseOverlay::Outline { selected } = state.overlay else {
            return;
        };

        if down {
            state.overlay = BrowseOverlay::Outline {
                selected: (selected + 1).min(count.saturating_sub(1)),
            };
        } else if up {
            state.overlay = BrowseOverlay::Outline {
                selected: selected.saturating_sub(1),
            };
        } else if confirm {
            let target = state.outline_symbols().get(selected).map(|s| s.line);
            state.overlay = BrowseOverlay::None;
            if let Some(line) = target {
                self.browse_push_jump();
                if let Some(state) = self.browse_state.as_mut() {
                    state.focus_line(line.saturating_sub(1));
                }
                self.state = AppState::RepoBrowseFile;
            }
        }
    }

    fn handle_browse_symbol_search_input(&mut self, key: event::KeyEvent) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        let BrowseOverlay::SymbolSearch {
            ref mut query,
            ref mut selected,
        } = state.overlay
        else {
            return;
        };

        // Typing wins over navigation: only arrows and Ctrl-n/p move the
        // selection, so `j` and `k` stay usable inside the query.
        match key.code {
            KeyCode::Esc => {
                state.overlay = BrowseOverlay::None;
                return;
            }
            KeyCode::Enter => {
                let query = query.clone();
                let selected = *selected;
                let target = state
                    .symbol_search_results(&query)
                    .into_iter()
                    .nth(selected)
                    .map(|(path, line, _)| (path, line));
                state.overlay = BrowseOverlay::None;
                if let Some((path, line)) = target {
                    self.browse_push_jump();
                    self.browse_open_path(&path, line.saturating_sub(1));
                }
                return;
            }
            KeyCode::Backspace => {
                query.pop();
                *selected = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                query.clear();
                *selected = 0;
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                *selected += 1;
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                *selected = selected.saturating_sub(1);
            }
            KeyCode::Down => *selected += 1,
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                query.push(c);
                *selected = 0;
            }
            _ => return,
        }

        state.clamp_symbol_search_selection();
    }

    pub(crate) fn open_browse_outline(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        if state.outline_symbols().is_empty() {
            // The pending check must come first. During a background load
            // `open` is a placeholder with no symbols, so reporting on the
            // index alone would claim the file has none when it has not been
            // read yet.
            let message = if state.open_is_pending() {
                "Still opening this file"
            } else {
                match state.index {
                    IndexState::Ready(_) => "No symbols in this file",
                    _ => "Symbol index is still building",
                }
            };
            state.status = Some(message.to_string());
            return;
        }

        // Land on the symbol containing the cursor rather than at the top —
        // the outline answers "where am I" as much as "where can I go".
        let cursor_line = state.cursor_line + 1;
        let selected = state
            .outline_symbols()
            .iter()
            .rposition(|symbol| symbol.line <= cursor_line)
            .unwrap_or(0);
        state.overlay = BrowseOverlay::Outline { selected };
    }

    pub(crate) fn open_browse_symbol_search(&mut self) {
        let Some(state) = self.browse_state.as_mut() else {
            return;
        };
        if state.index.ready().is_none() {
            state.status = Some("Symbol index is still building".to_string());
            return;
        }
        state.overlay = BrowseOverlay::SymbolSearch {
            query: String::new(),
            selected: 0,
        };
    }

    pub(crate) fn set_browse_status(&mut self, message: &str) {
        if let Some(state) = self.browse_state.as_mut() {
            state.status = Some(message.to_string());
        }
    }
}

impl super::browse::BrowseState {
    /// Keep the highlighted row inside the current result set.
    pub fn clamp_symbol_search_selection(&mut self) {
        let count = match self.overlay {
            BrowseOverlay::SymbolSearch { ref query, .. } => self.symbol_search_hits(query).len(),
            _ => return,
        };
        if let BrowseOverlay::SymbolSearch {
            ref mut selected, ..
        } = self.overlay
        {
            *selected = (*selected).min(count.saturating_sub(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::browse::{build_file_patch, BlameState, BrowseState, OpenFile, OpenLoad};
    use crate::symbols::{FileSymbols, Symbol, SymbolIndex, SymbolKind};
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// An app parked in the browser with a fake repository already listed.
    fn browsing_app(paths: &[&str]) -> App {
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(PathBuf::from("/repo"), AppState::FileList);
        state.set_paths(paths.iter().map(|p| p.to_string()).collect());
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;
        app
    }

    fn attach_open_file(app: &mut App, path: &str, source: &str, symbols: Vec<Symbol>) {
        let patch = build_file_patch(source);
        let state = app.browse_state.as_mut().expect("browse state");
        state.open = Some(OpenFile {
            path: path.to_string(),
            cache: crate::ui::diff_view::build_plain_diff_cache(&patch, 4),
            patch,
            lines: source.lines().map(str::to_string).collect(),
            symbols,
            viewable: true,
            notice: None,
        });
        state.sync_tree_to_open_file();
    }

    fn attach_index(app: &mut App, files: Vec<FileSymbols>) {
        let state = app.browse_state.as_mut().expect("browse state");
        state.index = IndexState::Ready(Arc::new(SymbolIndex::from_files(files)));
        state.refresh_open_file_symbols();
    }

    /// A browser rooted at a real directory holding `files`.
    ///
    /// Jumps that land in a *different* file go through `browse_open_path`,
    /// which reads from disk — the in-memory `attach_open_file` fixture cannot
    /// reach that branch.
    fn browsing_app_on_disk(root: &std::path::Path, files: &[(&str, &str)]) -> App {
        for (path, source) in files {
            let absolute = root.join(path);
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent).expect("fixture directory");
            }
            std::fs::write(&absolute, source).expect("fixture file");
        }

        let mut app = App::new_for_test();
        let mut state = BrowseState::new(root.to_path_buf(), AppState::FileList);
        state.set_paths(files.iter().map(|(path, _)| (*path).to_string()).collect());
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;
        app
    }

    fn symbol(name: &str, kind: SymbolKind, line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            line,
            column: 0,
            depth: 0,
        }
    }

    fn open_path(app: &App) -> &str {
        app.browse_state
            .as_ref()
            .and_then(|state| state.open.as_ref())
            .map_or("", |open| open.path.as_str())
    }

    /// Drive the browse background channels until the pending load lands.
    async fn settle_browse(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if matches!(
                app.browse_state.as_ref().map(|state| &state.open_load),
                None | Some(OpenLoad::Idle | OpenLoad::Failed { .. })
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("browse load never settled");
    }

    async fn settle_blame(app: &mut App) {
        for _ in 0..2_000 {
            app.poll_browse_updates();
            if matches!(
                app.browse_state.as_ref().map(|state| &state.blame),
                Some(BlameState::Ready { .. } | BlameState::Failed)
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("blame load never settled");
    }

    // ===== scenario: navigate the tree and leave =====

    #[test]
    fn test_scenario_move_through_tree_then_quit_back_to_caller() {
        let mut app = browsing_app(&["src/a.rs", "src/b.rs"]);
        let start = app
            .browse_state
            .as_ref()
            .map(|state| state.tree.selected_row)
            .unwrap();

        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        assert_eq!(
            app.browse_state.as_ref().unwrap().tree.selected_row,
            start + 1
        );

        app.handle_repo_browse_tree_input(press(KeyCode::Char('k')))
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().tree.selected_row, start);

        app.handle_repo_browse_tree_input(press(KeyCode::Char('q')))
            .unwrap();
        assert_eq!(app.state, AppState::FileList);
        assert!(app.browse_state.is_none());
    }

    #[tokio::test]
    async fn test_scenario_open_file_toggle_blame_on_poll_then_toggle_off() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Scenario Author",
                    "-c",
                    "user.email=scenario@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir.path())
                .output();
            match output {
                Ok(output) if output.status.success() => true,
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && std::env::var_os("CI").is_none() =>
                {
                    false
                }
                Ok(output) => panic!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&output.stderr)
                ),
                Err(error) => panic!("git {} failed: {error}", args.join(" ")),
            }
        };
        if !git(&["init"]) {
            return;
        }
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        assert!(git(&["add", "main.rs"]));
        assert!(git(&["commit", "-m", "baseline"]));

        let mut app = browsing_app_on_disk(dir.path(), &[("main.rs", "fn main() {}\n")]);
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;
        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Off
        ));

        let mut terminal = test_terminal();
        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut terminal)
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('b')), &mut terminal)
            .unwrap();
        assert!(matches!(
            app.browse_state.as_ref().unwrap().blame,
            BlameState::Loading { .. }
        ));

        settle_blame(&mut app).await;
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.blame,
            BlameState::Ready {
                ref path,
                ref gutter
            } if path == "main.rs" && gutter.len() == 1
        ));

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut terminal)
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('b')), &mut terminal)
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.blame, BlameState::Off));
        assert!(state.blame_receiver.is_none());
    }

    #[test]
    fn test_scenario_collapse_and_expand_a_directory() {
        let mut app = browsing_app(&["src/a.rs", "src/b.rs"]);
        // Row 0 is the `src/` directory row.
        assert_eq!(app.browse_state.as_ref().unwrap().tree.row_count(), 3);

        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().tree.row_count(), 1);
        assert_eq!(
            app.state,
            AppState::RepoBrowseTree,
            "collapsing is not opening"
        );

        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().tree.row_count(), 3);
    }

    // ===== scenario: filter the tree =====

    #[test]
    fn test_scenario_filter_then_cancel_restores_full_tree() {
        let mut app = browsing_app(&["src/app.rs", "src/ui.rs", "README.md"]);
        let full_rows = app.browse_state.as_ref().unwrap().tree.row_count();

        app.handle_repo_browse_tree_input(press(KeyCode::Char(' ')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Char('/')))
            .unwrap();
        for c in "ui".chars() {
            app.handle_repo_browse_tree_input(press(KeyCode::Char(c)))
                .unwrap();
        }
        assert_eq!(
            app.browse_state.as_ref().unwrap().tree.dump_tree(),
            "▼ src/\n  ui.rs"
        );

        app.handle_repo_browse_tree_input(press(KeyCode::Backspace))
            .unwrap();
        app.handle_repo_browse_tree_input(ctrl('u')).unwrap();
        assert_eq!(
            app.browse_state.as_ref().unwrap().tree.row_count(),
            full_rows
        );

        app.handle_repo_browse_tree_input(press(KeyCode::Esc))
            .unwrap();
        assert!(app.browse_state.as_ref().unwrap().filter.is_none());
        assert_eq!(
            app.browse_state.as_ref().unwrap().tree.row_count(),
            full_rows
        );
    }

    #[test]
    fn test_filter_input_swallows_navigation_keys() {
        let mut app = browsing_app(&["src/app.rs", "src/ui.rs"]);
        app.handle_repo_browse_tree_input(press(KeyCode::Char(' ')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Char('/')))
            .unwrap();
        // 'j' must type, not move the cursor, while the filter bar is active.
        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        assert_eq!(
            app.browse_state
                .as_ref()
                .unwrap()
                .filter
                .as_ref()
                .unwrap()
                .query,
            "j"
        );
    }

    // ===== scenario: read a file =====

    #[test]
    fn test_scenario_scroll_a_file_and_return_to_the_tree() {
        let mut app = browsing_app(&["src/a.rs"]);
        let source: String = (0..40).map(|i| format!("line {i}\n")).collect();
        attach_open_file(&mut app, "src/a.rs", &source, Vec::new());
        app.state = AppState::RepoBrowseFile;

        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 1);

        app.handle_repo_browse_file_input(press(KeyCode::Char('G')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 39);

        // `gg` returns to the top.
        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 0);

        app.handle_repo_browse_file_input(press(KeyCode::Char('h')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.state, AppState::RepoBrowseTree);
    }

    // ===== scenario: outline =====

    fn app_with_outline() -> App {
        let mut app = browsing_app(&["src/a.rs"]);
        attach_open_file(
            &mut app,
            "src/a.rs",
            "struct App;\nimpl App {\n  fn run() {}\n}\n",
            Vec::new(),
        );
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/a.rs".to_string(),
                symbols: vec![
                    Symbol {
                        name: "App".to_string(),
                        kind: SymbolKind::Class,
                        line: 1,
                        column: 7,
                        depth: 0,
                    },
                    Symbol {
                        name: "run".to_string(),
                        kind: SymbolKind::Method,
                        line: 3,
                        column: 5,
                        depth: 1,
                    },
                ],
            }],
        );
        app.state = AppState::RepoBrowseFile;
        app
    }

    #[test]
    fn test_scenario_outline_jumps_to_a_symbol() {
        let mut app = app_with_outline();

        app.handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut test_terminal())
            .unwrap();
        assert_eq!(
            app.browse_state.as_ref().unwrap().overlay,
            BrowseOverlay::Outline { selected: 0 }
        );

        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Enter), &mut test_terminal())
            .unwrap();

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(state.cursor_line, 2, "jumped to `fn run` on line 3");
        assert_eq!(state.jump_stack.len(), 1, "jump is undoable");
    }

    #[test]
    fn test_outline_opens_on_the_symbol_under_the_cursor() {
        let mut app = app_with_outline();
        // Walk the cursor onto `fn run` rather than assigning `cursor_line`,
        // so the pre-selection is reached the way a reader reaches it.
        for _ in 0..2 {
            app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
                .unwrap();
        }
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 2);

        app.handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut test_terminal())
            .unwrap();
        assert_eq!(
            app.browse_state.as_ref().unwrap().overlay,
            BrowseOverlay::Outline { selected: 1 }
        );
    }

    #[test]
    fn test_outline_escape_closes_without_moving() {
        let mut app = app_with_outline();
        app.handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Esc), &mut test_terminal())
            .unwrap();

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(state.cursor_line, 0);
        assert!(state.jump_stack.is_empty());
    }

    #[test]
    fn test_outline_without_symbols_reports_instead_of_opening_empty() {
        let mut app = browsing_app(&["notes.txt"]);
        attach_open_file(&mut app, "notes.txt", "plain text\n", Vec::new());
        app.state = AppState::RepoBrowseFile;

        app.handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut test_terminal())
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(
            state.status.as_deref(),
            Some("Symbol index is still building")
        );
    }

    // ===== scenario: symbol search =====

    #[test]
    fn test_scenario_symbol_search_typing_then_jump() {
        let mut app = app_with_outline();

        app.handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut test_terminal())
            .unwrap();
        for c in "run".chars() {
            app.handle_repo_browse_file_input(press(KeyCode::Char(c)), &mut test_terminal())
                .unwrap();
        }
        assert_eq!(
            app.browse_state.as_ref().unwrap().overlay,
            BrowseOverlay::SymbolSearch {
                query: "run".to_string(),
                selected: 0,
            }
        );

        app.handle_repo_browse_file_input(press(KeyCode::Enter), &mut test_terminal())
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(state.cursor_line, 2);
    }

    #[test]
    fn test_symbol_search_j_and_k_type_rather_than_navigate() {
        let mut app = app_with_outline();
        app.handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();

        let BrowseOverlay::SymbolSearch { ref query, .. } =
            app.browse_state.as_ref().unwrap().overlay
        else {
            panic!("overlay should still be symbol search");
        };
        assert_eq!(query, "j");
    }

    #[test]
    fn test_symbol_search_selection_is_clamped_to_results() {
        let mut app = app_with_outline();
        app.handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut test_terminal())
            .unwrap();
        // Two symbols exist, but only one matches "run".
        for _ in 0..5 {
            app.handle_repo_browse_file_input(press(KeyCode::Down), &mut test_terminal())
                .unwrap();
        }
        for c in "run".chars() {
            app.handle_repo_browse_file_input(press(KeyCode::Char(c)), &mut test_terminal())
                .unwrap();
        }
        assert_eq!(
            app.browse_state.as_ref().unwrap().overlay,
            BrowseOverlay::SymbolSearch {
                query: "run".to_string(),
                selected: 0,
            }
        );
    }

    /// The clamp bounds the cursor by the same set the overlay draws.
    ///
    /// With fewer matches than the cap the two are indistinguishable, which is
    /// what the sibling test above exercises. Only a query that overflows the
    /// cap can tell them apart: a clamp with a cap of its own would stop the
    /// cursor short of rows the user can plainly see and never let them open
    /// one.
    #[test]
    fn test_the_selection_clamp_reaches_every_row_the_overlay_draws() {
        let mut app = app_with_outline();
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/a.rs".to_string(),
                symbols: (0..crate::app::browse::MAX_SYMBOL_SEARCH_RESULTS + 50)
                    .map(|n| Symbol {
                        name: format!("many_{n:04}"),
                        kind: SymbolKind::Function,
                        line: n + 1,
                        column: 0,
                        depth: 0,
                    })
                    .collect(),
            }],
        );

        app.handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut test_terminal())
            .unwrap();
        for c in "many".chars() {
            app.handle_repo_browse_file_input(press(KeyCode::Char(c)), &mut test_terminal())
                .unwrap();
        }
        let drawn = app
            .browse_state
            .as_ref()
            .unwrap()
            .symbol_search_hits("many")
            .len();
        assert_eq!(drawn, crate::app::browse::MAX_SYMBOL_SEARCH_RESULTS);

        for _ in 0..drawn + 20 {
            app.handle_repo_browse_file_input(press(KeyCode::Down), &mut test_terminal())
                .unwrap();
        }

        let BrowseOverlay::SymbolSearch { selected, .. } =
            app.browse_state.as_ref().unwrap().overlay
        else {
            panic!("the overlay must still be symbol search");
        };
        assert_eq!(
            selected,
            drawn - 1,
            "the cursor stopped short of rows the overlay draws, so they can be \
             seen but never opened"
        );
    }

    #[test]
    fn test_symbol_search_refuses_to_open_before_the_index_is_ready() {
        let mut app = browsing_app(&["src/a.rs"]);
        app.handle_repo_browse_tree_input(press(KeyCode::Char('s')))
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(
            state.status.as_deref(),
            Some("Symbol index is still building")
        );
    }

    // ===== scenario: go to definition and jump back =====

    #[test]
    fn test_go_to_definition_reports_when_index_is_not_ready() {
        let mut app = browsing_app(&["src/a.rs"]);
        attach_open_file(&mut app, "src/a.rs", "let x = alpha();\n", Vec::new());
        app.state = AppState::RepoBrowseFile;

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('d')), &mut test_terminal())
            .unwrap();

        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("Symbol index is still building")
        );
    }

    /// A pending load leaves `open` holding a placeholder with no content and
    /// no symbols. Both `o` and `gd` answer questions about the open file, so
    /// both must say the file is still opening rather than reporting on the
    /// placeholder as if it were the file.
    #[test]
    fn test_outline_and_definition_do_not_report_on_a_file_still_being_read() {
        for key in ['o', 'd'] {
            let mut app = app_with_outline();
            attach_index(
                &mut app,
                vec![FileSymbols {
                    path: "src/a.rs".to_string(),
                    symbols: Vec::new(),
                }],
            );
            let state = app.browse_state.as_mut().unwrap();
            state.open_load = OpenLoad::Pending {
                path: "src/a.rs".to_string(),
                line: 0,
                scroll: None,
                cancel: tokio_util::sync::CancellationToken::new(),
            };
            state.open = None;
            state.status = None;
            app.state = AppState::RepoBrowseFile;

            if key == 'd' {
                app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
                    .unwrap();
            }
            app.handle_repo_browse_file_input(press(KeyCode::Char(key)), &mut test_terminal())
                .unwrap();

            assert_eq!(
                app.browse_state.as_ref().unwrap().status.as_deref(),
                Some("Still opening this file"),
                "key {key} reported on a placeholder instead of the pending load"
            );
        }
    }

    /// The pending check must sit *above* the index check, not merely exist.
    ///
    /// With a ready index both orderings answer the same thing, so the sibling
    /// test above cannot see the order. Here both conditions hold at once —
    /// the file is still being read *and* the index is not ready — and only the
    /// documented order produces the pending message.
    #[test]
    fn test_pending_load_outranks_an_unbuilt_index_for_both_keys() {
        for key in ['o', 'd'] {
            let mut app = browsing_app(&["src/a.rs"]);
            let state = app.browse_state.as_mut().unwrap();
            assert!(
                state.index.ready().is_none(),
                "this test is only meaningful while the index is unbuilt"
            );
            state.open_load = OpenLoad::Pending {
                path: "src/a.rs".to_string(),
                line: 0,
                scroll: None,
                cancel: tokio_util::sync::CancellationToken::new(),
            };
            state.open = None;
            state.status = None;
            app.state = AppState::RepoBrowseFile;

            if key == 'd' {
                app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
                    .unwrap();
            }
            app.handle_repo_browse_file_input(press(KeyCode::Char(key)), &mut test_terminal())
                .unwrap();

            assert_eq!(
                app.browse_state.as_ref().unwrap().status.as_deref(),
                Some("Still opening this file"),
                "key {key} let the index check answer for a file it has not read"
            );
        }
    }

    #[test]
    fn test_go_to_definition_reports_when_nothing_matches() {
        let mut app = app_with_outline();
        app.browse_state.as_mut().unwrap().cursor_line = 0;
        // Line 1 is `struct App;` — `App` resolves, so point at a line with no
        // indexed identifier instead.
        attach_open_file(
            &mut app,
            "src/a.rs",
            "let q = unknown_thing();\n",
            Vec::new(),
        );
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/a.rs".to_string(),
                symbols: Vec::new(),
            }],
        );
        app.state = AppState::RepoBrowseFile;

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('d')), &mut test_terminal())
            .unwrap();

        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("No definition found")
        );
    }

    #[test]
    fn test_jump_back_returns_to_the_previous_position() {
        let mut app = app_with_outline();
        app.handle_repo_browse_file_input(press(KeyCode::Char('o')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Enter), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 2);

        app.handle_repo_browse_file_input(ctrl('o'), &mut test_terminal())
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.cursor_line, 0);
        assert!(state.jump_stack.is_empty());
    }

    #[tokio::test]
    async fn test_scenario_go_to_definition_crosses_files_and_ctrl_o_returns() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[
                ("src/lib.rs", "pub struct Config;\n\npub fn helper() {}\n"),
                ("src/main.rs", "fn main() {\n    helper();\n}\n"),
            ],
        );
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/lib.rs".to_string(),
                symbols: vec![
                    symbol("Config", SymbolKind::Class, 1),
                    symbol("helper", SymbolKind::Function, 3),
                ],
            }],
        );

        // Row 0 is `src/`, row 1 `lib.rs`, row 2 `main.rs`.
        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(open_path(&app), "src/main.rs");
        settle_browse(&mut app).await;

        // Put the cursor on `    helper();`.
        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 1);

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('d')), &mut test_terminal())
            .unwrap();
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        let open = state.open.as_ref().unwrap();
        assert_eq!(open.path, "src/lib.rs", "jumped into the other file");
        assert_eq!(
            open.lines,
            vec!["pub struct Config;", "", "pub fn helper() {}"],
            "the target was read from disk, not reused from the caller"
        );
        assert_eq!(state.cursor_line, 2, "`pub fn helper` is line 3, 0-based 2");
        assert_eq!(state.status, None, "a successful jump reports nothing");
        assert_eq!(state.jump_stack.len(), 1, "the jump is undoable");
        assert_eq!(state.tree.selected_row, 1, "the tree followed the jump");
        assert_eq!(app.state, AppState::RepoBrowseFile);

        app.handle_repo_browse_file_input(ctrl('o'), &mut test_terminal())
            .unwrap();
        settle_browse(&mut app).await;
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(open_path(&app), "src/main.rs", "Ctrl-o reopened the caller");
        assert_eq!(state.cursor_line, 1, "back on the call site");
        assert!(state.jump_stack.is_empty());
    }

    /// Pins the granularity the README documents: `gd` is a *line* operation.
    ///
    /// `BrowseState` tracks `cursor_line` and no column, so a line naming two
    /// indexed symbols resolves in reading order — `Config` here, never
    /// `helper`. Changing that without changing the README is a regression.
    #[tokio::test]
    async fn test_go_to_definition_resolves_the_first_indexed_identifier_on_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[
                ("src/lib.rs", "pub struct Config;\n\npub fn helper() {}\n"),
                ("src/main.rs", "fn main() {\n    Config::helper();\n}\n"),
            ],
        );
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/lib.rs".to_string(),
                symbols: vec![
                    symbol("Config", SymbolKind::Class, 1),
                    symbol("helper", SymbolKind::Function, 3),
                ],
            }],
        );

        // Row 0 is `src/`, row 1 `lib.rs`, row 2 `main.rs`.
        for _ in 0..2 {
            app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
                .unwrap();
        }
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;
        // Put the cursor on `    Config::helper();`.
        app.handle_repo_browse_file_input(press(KeyCode::Char('j')), &mut test_terminal())
            .unwrap();
        assert_eq!(app.browse_state.as_ref().unwrap().cursor_line, 1);

        app.handle_repo_browse_file_input(press(KeyCode::Char('g')), &mut test_terminal())
            .unwrap();
        app.handle_repo_browse_file_input(press(KeyCode::Char('d')), &mut test_terminal())
            .unwrap();
        settle_browse(&mut app).await;

        assert_eq!(open_path(&app), "src/lib.rs");
        assert_eq!(
            app.browse_state.as_ref().unwrap().cursor_line,
            0,
            "`Config` comes first on the line, so `helper` is never tried"
        );
    }

    #[tokio::test]
    async fn test_scenario_symbol_search_jumps_into_a_file_that_is_not_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = browsing_app_on_disk(
            dir.path(),
            &[
                ("src/lib.rs", "pub struct Config;\n\npub fn helper() {}\n"),
                ("src/main.rs", "fn main() {\n    helper();\n}\n"),
            ],
        );
        attach_index(
            &mut app,
            vec![FileSymbols {
                path: "src/lib.rs".to_string(),
                symbols: vec![
                    symbol("Config", SymbolKind::Class, 1),
                    symbol("helper", SymbolKind::Function, 3),
                ],
            }],
        );
        app.browse_open_path("src/main.rs", 0);
        settle_browse(&mut app).await;

        app.handle_repo_browse_file_input(press(KeyCode::Char('s')), &mut test_terminal())
            .unwrap();
        for c in "helper".chars() {
            app.handle_repo_browse_file_input(press(KeyCode::Char(c)), &mut test_terminal())
                .unwrap();
        }
        app.handle_repo_browse_file_input(press(KeyCode::Enter), &mut test_terminal())
            .unwrap();
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.overlay, BrowseOverlay::None);
        assert_eq!(open_path(&app), "src/lib.rs", "left the file it started in");
        assert_eq!(state.cursor_line, 2);
        assert_eq!(state.jump_stack.len(), 1);
        assert_eq!(
            state.tree.selected_row, 1,
            "the tree follows the file the overlay opened"
        );
    }

    #[test]
    fn test_jump_back_with_empty_stack_reports() {
        let mut app = app_with_outline();
        app.handle_repo_browse_file_input(ctrl('o'), &mut test_terminal())
            .unwrap();
        assert_eq!(
            app.browse_state.as_ref().unwrap().status.as_deref(),
            Some("No position to jump back to")
        );
    }

    // ===== opening from the tree =====

    #[tokio::test]
    async fn test_open_a_file_from_the_tree_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "pub fn alpha() {}\n").unwrap();

        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["src/a.rs".to_string()]);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;

        // Row 0 is `src/`; move onto the file before opening it.
        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();

        assert_eq!(app.state, AppState::RepoBrowseFile);
        let placeholder = app.browse_state.as_ref().unwrap().open.as_ref().unwrap();
        assert_eq!(placeholder.path, "src/a.rs");
        assert!(!placeholder.viewable);
        assert!(placeholder
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Loading")));

        settle_browse(&mut app).await;

        let open = app.browse_state.as_ref().unwrap().open.as_ref().unwrap();
        assert!(open.viewable);
        assert_eq!(open.lines, vec!["pub fn alpha() {}"]);
    }

    #[tokio::test]
    async fn test_missing_file_stays_on_failure_pane_then_returns_to_a_usable_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("live.rs"), "pub fn live() {}\n").unwrap();
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["gone.rs".to_string(), "live.rs".to_string()]);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;

        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;

        assert_eq!(app.state, AppState::RepoBrowseFile);
        let state = app.browse_state.as_ref().unwrap();
        let status = state.status.as_deref().unwrap();
        assert!(status.starts_with("gone.rs:"), "{status}");
        let open = state.open.as_ref().expect("failure pane");
        assert_eq!(open.path, "gone.rs");
        assert!(!open.viewable);
        let notice = open.notice.as_deref().expect("failure notice");
        assert!(notice.contains("gone.rs:"), "{notice}");

        app.handle_repo_browse_file_input(press(KeyCode::Esc), &mut test_terminal())
            .unwrap();
        assert_eq!(app.state, AppState::RepoBrowseTree);

        app.handle_repo_browse_tree_input(press(KeyCode::Char('j')))
            .unwrap();
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        settle_browse(&mut app).await;

        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(app.state, AppState::RepoBrowseFile);
        assert_eq!(state.open.as_ref().unwrap().path, "live.rs");
        assert_eq!(
            state.open.as_ref().unwrap().lines,
            vec!["pub fn live() {}"],
            "the tree remained usable after leaving the failure pane"
        );
    }

    /// A throwaway terminal for handlers that take one but do not draw.
    fn test_terminal() -> Terminal<CrosstermBackend<Stdout>> {
        Terminal::new(CrosstermBackend::new(std::io::stdout())).expect("terminal")
    }
}
