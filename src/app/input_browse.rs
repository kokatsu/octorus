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

use super::browse::{BrowseOverlay, IndexState, MAX_SYMBOL_SEARCH_RESULTS};
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

    /// Resolve `gg` / `gd` / `gf`. Returns `true` when the key was consumed.
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

        let starts_sequence = self.key_could_match_sequence(key, &kb.go_to_definition)
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
            let message = match state.index {
                IndexState::Ready(_) => "No symbols in this file",
                _ => "Symbol index is still building",
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
        let count = self.index.ready().map_or(0, |index| {
            let BrowseOverlay::SymbolSearch { ref query, .. } = self.overlay else {
                return 0;
            };
            index.search(query, MAX_SYMBOL_SEARCH_RESULTS).len()
        });
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
    use crate::app::browse::{build_file_patch, BrowseState, OpenFile};
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
        app.browse_state.as_mut().unwrap().cursor_line = 2;

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
        let open = app.browse_state.as_ref().unwrap().open.as_ref().unwrap();
        assert_eq!(open.path, "src/a.rs");
        assert_eq!(open.lines, vec!["pub fn alpha() {}"]);
    }

    #[test]
    fn test_opening_a_deleted_file_reports_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new_for_test();
        let mut state = BrowseState::new(dir.path().to_path_buf(), AppState::FileList);
        state.set_paths(vec!["gone.rs".to_string()]);
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;

        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();

        assert_eq!(app.state, AppState::RepoBrowseTree);
        let status = app.browse_state.as_ref().unwrap().status.clone().unwrap();
        assert!(status.starts_with("gone.rs:"), "{status}");
    }

    /// A throwaway terminal for handlers that take one but do not draw.
    fn test_terminal() -> Terminal<CrosstermBackend<Stdout>> {
        Terminal::new(CrosstermBackend::new(std::io::stdout())).expect("terminal")
    }
}
