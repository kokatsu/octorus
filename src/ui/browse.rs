//! Repository Browser rendering: file tree, file content, and overlays.

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};

use unicode_width::UnicodeWidthStr;

use crate::app::browse::{BrowseOverlay, BrowseState};
use crate::app::{App, AppState, CachedDiffLine, DiffCache, LoadState, TreeRow};
use crate::diff::LineType;
use crate::symbols::Symbol;

/// Narrowest the line-number gutter ever gets.
///
/// Five columns hold every line number up to 99,999. Beyond that the gutter
/// grows rather than overflowing — see [`gutter_width`].
const LINE_NUMBER_WIDTH: usize = 5;

pub fn render(frame: &mut Frame, app: &mut App) {
    let zen = app.zen_mode;
    let constraints = if zen {
        vec![Constraint::Min(0)]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    let body = if zen { chunks[0] } else { chunks[1] };

    if !zen {
        render_header(frame, app, chunks[0]);
    }

    let tree_focused = app.state == AppState::RepoBrowseTree;
    let left_width = app.config.layout.left_panel_width;
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(left_width),
            Constraint::Percentage(100 - left_width),
        ])
        .split(body);

    render_tree(frame, app, panes[0], tree_focused);
    render_content(frame, app, panes[1], !tree_focused);

    if !zen {
        render_footer(frame, app, chunks[2]);
    }

    render_overlay(frame, app);
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.browse_state.as_ref() else {
        return;
    };

    let root = state
        .repo_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| state.repo_root.to_string_lossy().to_string());

    let file_count = match state.paths {
        LoadState::Loaded(ref paths) => format!("{} files", paths.len()),
        LoadState::Error(_) => "unavailable".to_string(),
        _ => "loading…".to_string(),
    };

    let header = Paragraph::new(Line::from(vec![
        Span::styled("Repo Browse", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" - "),
        Span::styled(root, Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(file_count, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(
            state.index.label(),
            Style::default().fg(match state.index {
                crate::app::browse::IndexState::Ready(_) => Color::Green,
                crate::app::browse::IndexState::Failed => Color::Red,
                _ => Color::Yellow,
            }),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}

fn render_tree(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let Some(state) = app.browse_state.as_ref() else {
        return;
    };

    let border_style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    if let LoadState::Error(ref message) = state.paths {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            format!("  {message}"),
            Style::default().fg(Color::Red),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title("Files"),
        );
        frame.render_widget(paragraph, area);
        return;
    }

    if matches!(state.paths, LoadState::Loading | LoadState::NotLoaded) {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            format!("  {} Listing files…", app.spinner_char()),
            Style::default().fg(Color::DarkGray),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title("Files"),
        );
        frame.render_widget(paragraph, area);
        return;
    }

    let inner_height = area.height.saturating_sub(2) as usize;
    let offset = scroll_offset(
        state.tree.selected_row,
        state.tree.row_count(),
        inner_height,
    );

    let items: Vec<ListItem> = state
        .tree
        .visible_rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(inner_height)
        .map(|(row_index, row)| {
            let selected = row_index == state.tree.selected_row;
            let (text, base) = match row {
                TreeRow::Dir {
                    path,
                    depth,
                    expanded,
                } => {
                    let name = path.rsplit_once('/').map(|(_, n)| n).unwrap_or(path);
                    (
                        format!(
                            "{}{} {}/",
                            "  ".repeat(*depth),
                            if *expanded { "▼" } else { "▶" },
                            name
                        ),
                        Style::default().fg(Color::Blue),
                    )
                }
                TreeRow::File { index, depth } => {
                    let path = state
                        .all_paths()
                        .get(*index)
                        .map(String::as_str)
                        .unwrap_or("?");
                    let name = path.rsplit_once('/').map(|(_, n)| n).unwrap_or(path);
                    let open = state
                        .open
                        .as_ref()
                        .is_some_and(|open| open.path.as_str() == path);
                    let style = if open {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default()
                    };
                    (format!("{}  {}", "  ".repeat(*depth), name), style)
                }
            };

            let style = if selected {
                base.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
            } else {
                base
            };
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();

    let title = match state.filter {
        Some(ref filter) if filter.input_active => format!("Filter: {}_", filter.query),
        Some(ref filter) if !filter.query.is_empty() => format!("Files (/{})", filter.query),
        _ => "Files".to_string(),
    };

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
    );
    frame.render_widget(list, area);
}

fn render_content(frame: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let bg_color = app.config.diff.bg_color;
    let spinner = app.spinner_char().to_string();
    let Some(state) = app.browse_state.as_mut() else {
        return;
    };

    let border_style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    let Some(open) = state.open.as_ref() else {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            "  Select a file to view it.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title("Preview"),
        );
        frame.render_widget(paragraph, area);
        return;
    };

    if let Some(ref notice) = open.notice {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            format!("  {notice}"),
            Style::default().fg(Color::DarkGray),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(open.path.clone()),
        );
        frame.render_widget(paragraph, area);
        return;
    }

    let inner_height = area.height.saturating_sub(2) as usize;
    state.clamp_scroll(inner_height);

    let state = app.browse_state.as_ref().expect("browse state");
    let open = state.open.as_ref().expect("open file");

    let window = content_window(&open.cache, state.scroll_offset, inner_height);
    let total = window.total;
    let lines = content_lines(&window, state.cursor_line, bg_color);

    let building = matches!(state.index, crate::app::browse::IndexState::Building);
    let title = if open.cache.highlighted || !building {
        format!("{} ({}/{})", open.path, state.cursor_line + 1, total.max(1))
    } else {
        format!("{} {}", open.path, spinner)
    };

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
    );
    frame.render_widget(paragraph, area);

    if total > inner_height {
        let max_scroll = total.saturating_sub(inner_height);
        let mut scrollbar_state = ScrollbarState::new(max_scroll).position(state.scroll_offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

/// The visible run of file-content lines inside a browse pseudo-patch cache.
struct ContentWindow<'a> {
    /// Cache the window borrows from; owns the interned span text.
    cache: &'a DiffCache,
    /// Content lines in the whole file, i.e. the file's line count.
    total: usize,
    /// 0-based file line rendered as `lines[0]`.
    first_line: usize,
    /// Borrowed view of exactly the lines the viewport shows.
    lines: &'a [CachedDiffLine],
}

/// Borrow the viewport's slice of `cache` without walking the rest of the file.
///
/// `build_file_patch` emits a single `@@` header and then one context line per
/// source line — every content line carries a leading space, so a literal `@@`
/// line in the source is never classified as a header. The headers are therefore
/// a prefix, and `cache.lines[content_start + i]` is file line `i`. Locating that
/// prefix costs one probe per header line plus one, so the whole render stays
/// O(viewport) on a 300,000-line file.
///
/// `test_content_window_finds_the_content_start_by_prefix_not_by_filtering`
/// protects the contiguous-slice shape. The `browse_render` group in
/// `benches/ui_rendering.rs` measures the O(viewport) cost itself.
fn content_window(cache: &DiffCache, scroll_offset: usize, height: usize) -> ContentWindow<'_> {
    let content_start = cache
        .lines
        .iter()
        .position(|line| line.line_type != LineType::Header)
        .unwrap_or(cache.lines.len());

    let total = cache.lines.len() - content_start;
    let first_line = scroll_offset.min(total);
    let end = (first_line + height).min(total);
    ContentWindow {
        cache,
        total,
        first_line,
        lines: &cache.lines[content_start + first_line..content_start + end],
    }
}

/// Columns the line-number gutter needs for a file of `total` lines.
///
/// `{:>5}` does not truncate — it pads to *at least* five columns — so a fixed
/// five-column gutter silently widened to six on line 100,000 and pushed that
/// one line's text a column right of every other line. `MAX_VIEWABLE_FILE_LINES`
/// is an inclusive cap, so a 100,000-line file opens and reaches exactly that
/// number: 99,999 is the last five-digit line and 100,000 the first six-digit
/// one. Widening the whole gutter keeps the column straight without hiding a
/// line number behind a truncation.
fn gutter_width(total: usize) -> usize {
    let digits = total.max(1).ilog10() as usize + 1;
    digits.max(LINE_NUMBER_WIDTH)
}

/// Build the viewport's ratatui lines: a line-number gutter plus the cached spans.
///
/// The span text stays borrowed from the cache's interner — the cache already owns
/// every string, and copying them would allocate once per span on every keystroke.
/// A copy renders the identical frame, so `test_content_lines_borrow_their_text`
/// asserts the `Cow` variant rather than the output.
fn content_lines<'a>(
    window: &ContentWindow<'a>,
    cursor_line: usize,
    bg_color: bool,
) -> Vec<Line<'a>> {
    let width = gutter_width(window.total);
    window
        .lines
        .iter()
        .enumerate()
        .map(|(offset, cached)| {
            let line_index = window.first_line + offset;
            let is_cursor = line_index == cursor_line;

            let mut spans = Vec::with_capacity(cached.spans.len() + 1);
            spans.push(Span::styled(
                format!("{:>width$} ", line_index + 1),
                Style::default().fg(if is_cursor {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
            ));

            for (index, span) in cached.spans.iter().enumerate() {
                let text = window.cache.resolve(span.content);
                // Strip the pseudo-patch's leading context marker.
                let text = if index == 0 {
                    text.strip_prefix(' ').unwrap_or(text)
                } else {
                    text
                };
                let mut style = span.style;
                if is_cursor && bg_color {
                    style = style.bg(Color::Rgb(48, 48, 64));
                }
                spans.push(Span::styled(text, style));
            }
            Line::from(spans)
        })
        .collect()
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let kb = &app.config.keybindings;
    let status = app
        .browse_state
        .as_ref()
        .and_then(|state| state.status.clone());

    let text = match status {
        Some(message) => message,
        None if app.state == AppState::RepoBrowseTree => format!(
            " {} open | {} filter | {} symbol search | {} back",
            kb.open_panel.display(),
            kb.filter.display(),
            kb.symbol_search.display(),
            kb.quit.display(),
        ),
        None => format!(
            " {} outline | {} symbol search | {} definition | {} editor | {} back",
            kb.symbol_outline.display(),
            kb.symbol_search.display(),
            kb.go_to_definition.display(),
            kb.go_to_file.display(),
            kb.quit.display(),
        ),
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

fn render_overlay(frame: &mut Frame, app: &App) {
    let Some(state) = app.browse_state.as_ref() else {
        return;
    };

    match state.overlay {
        BrowseOverlay::None => {}
        BrowseOverlay::Outline { selected } => render_outline(frame, state, selected),
        BrowseOverlay::SymbolSearch {
            ref query,
            selected,
        } => render_symbol_search(frame, state, query, selected),
    }
}

/// Clear `area` for an overlay, blanking the double-width glyph it lands inside.
///
/// [`Clear`] resets only the cells inside `area`, which is not enough when the
/// screen underneath holds CJK or other double-width text: a glyph starting in
/// the column immediately left of `area` still occupies the overlay's first
/// column. Ratatui's buffer diff skips the cell after a double-width symbol, so
/// the overlay's left border is not merely painted over — it is never emitted,
/// and the terminal shows the glyph's second half in its place.
///
/// The right edge needs no such repair: a glyph whose first half the overlay
/// overwrites leaves behind a continuation cell that already holds a space, so
/// the border and the cell after it both render normally.
///
/// A text snapshot cannot see any of this — a continuation cell holds a space
/// either way, so the rendered text is identical whether the frame survives or
/// not. `test_overlay_left_border_survives_a_wide_glyph_straddling_it` asserts
/// on cells instead.
fn clear_overlay_area(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);

    let Some(left) = area.x.checked_sub(1) else {
        return;
    };
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        if let Some(cell) = buffer.cell_mut((left, y)) {
            if cell.symbol().width() > 1 {
                cell.set_symbol(" ");
            }
        }
    }
}

fn render_outline(frame: &mut Frame, state: &BrowseState, selected: usize) {
    let area = overlay_rect(frame.area(), 60, 70);
    clear_overlay_area(frame, area);

    let inner_height = area.height.saturating_sub(2) as usize;
    let symbols = state.outline_symbols();
    let offset = scroll_offset(selected, symbols.len(), inner_height);

    let items: Vec<ListItem> = symbols
        .iter()
        .enumerate()
        .skip(offset)
        .take(inner_height)
        .map(|(index, symbol)| ListItem::new(outline_row(symbol, index == selected)))
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!("Outline ({} symbols)", symbols.len()))
            .title_bottom(Line::from(Span::styled(
                " j/k move | Enter jump | Esc close ",
                Style::default().fg(Color::DarkGray),
            ))),
    );
    frame.render_widget(list, area);
}

fn outline_row(symbol: &Symbol, selected: bool) -> Line<'static> {
    let style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(Span::styled(
        format!(
            " {}{} {}  :{}",
            "  ".repeat(symbol.depth),
            symbol.kind.glyph(),
            symbol.name,
            symbol.line
        ),
        style,
    ))
}

fn render_symbol_search(frame: &mut Frame, state: &BrowseState, query: &str, selected: usize) {
    let area = overlay_rect(frame.area(), 80, 70);
    clear_overlay_area(frame, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    let input = Paragraph::new(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::raw(query.to_string()),
        Span::styled("_", Style::default().fg(Color::Yellow)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title("Symbol search"),
    );
    frame.render_widget(input, rows[0]);

    let hits = state.symbol_search_hits(query);
    let inner_height = rows[1].height.saturating_sub(2) as usize;
    let offset = scroll_offset(selected, hits.len(), inner_height);

    let items: Vec<ListItem> = hits
        .iter()
        .enumerate()
        .skip(offset)
        .take(inner_height)
        .map(|(index, hit)| {
            let style = if index == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let label = hit.search_label();
            ListItem::new(Line::from(Span::styled(format!(" {label}"), style)))
        })
        .collect();

    let title = if query.is_empty() {
        format!(
            "Type to search {} symbols",
            state.index.ready().map_or(0, |i| i.symbol_count())
        )
    } else {
        format!("{} matches", hits.len())
    };

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title)
            .title_bottom(Line::from(Span::styled(
                " ↑/↓ or Ctrl-p/n move | Enter jump | Esc close ",
                Style::default().fg(Color::DarkGray),
            ))),
    );
    frame.render_widget(list, rows[1]);
}

/// Keep `selected` visible inside a window `height` rows tall.
fn scroll_offset(selected: usize, total: usize, height: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    let max = total - height;
    selected.saturating_sub(height / 2).min(max)
}

/// A centred rectangle sized as a percentage of `area`.
fn overlay_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let width = (area.width * width_percent / 100).max(20).min(area.width);
    let height = (area.height * height_percent / 100).max(5).min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::browse::{
        build_file_patch, BrowseState, IndexState, OpenFile, MAX_SYMBOL_SEARCH_RESULTS,
        MAX_VIEWABLE_FILE_LINES,
    };
    use crate::config::Config;
    use crate::filter::ListFilter;
    use crate::symbols::{FileSymbols, Symbol, SymbolIndex, SymbolKind};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};
    use ratatui::Terminal;
    use std::borrow::Cow;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn render_buffer(app: &mut App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_at(app: &mut App, width: u16, height: u16) -> String {
        let buf = render_buffer(app, width, height);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn cache_of(source: &str) -> crate::app::DiffCache {
        crate::ui::diff_view::build_plain_diff_cache(&build_file_patch(source), 4)
    }

    fn numbered_source(lines: usize) -> String {
        (1..=lines).map(|line| format!("line {line}\n")).collect()
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_binding(binding: crate::keybinding::KeyBinding) -> KeyEvent {
        KeyEvent::new(binding.code.to_keycode(), binding.modifiers.to_crossterm())
    }

    fn app_with_browse(paths: &[&str]) -> App {
        let mut app = App::new_for_test();
        app.config = Config::default();
        let mut state = BrowseState::new(PathBuf::from("/tmp/demo"), AppState::FileList);
        state.set_paths(paths.iter().map(|p| p.to_string()).collect());
        app.browse_state = Some(state);
        app.state = AppState::RepoBrowseTree;
        app
    }

    fn open_file(state: &mut BrowseState, path: &str, source: &str) {
        let patch = build_file_patch(source);
        state.open = Some(OpenFile {
            path: path.to_string(),
            cache: crate::ui::diff_view::build_plain_diff_cache(&patch, 4),
            patch,
            lines: source.lines().map(str::to_string).collect(),
            symbols: Vec::new(),
            viewable: true,
            notice: None,
        });
    }

    #[test]
    fn test_render_tree_while_listing() {
        let mut app = App::new_for_test();
        app.browse_state = Some(BrowseState::new(
            PathBuf::from("/tmp/demo"),
            AppState::FileList,
        ));
        app.state = AppState::RepoBrowseTree;
        let out = render_at(&mut app, 80, 12);
        assert!(out.contains("Listing files"), "{out}");
        assert!(out.contains("Select a file to view it."), "{out}");
    }

    #[test]
    fn test_render_tree_and_empty_preview() {
        let mut app = app_with_browse(&["src/app.rs", "src/ui/mod.rs", "README.md"]);
        assert_snapshot!(render_at(&mut app, 80, 14), @r"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  3 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┌Preview───────────────────────────────────────────┐
        │▼ src/                    ││  Select a file to view it.                       │
        │  ▼ ui/                   ││                                                  │
        │      mod.rs              ││                                                  │
        │    app.rs                ││                                                  │
        │  README.md               ││                                                  │
        │                          ││                                                  │
        │                          ││                                                  │
        │                          ││                                                  │
        └──────────────────────────┘└──────────────────────────────────────────────────┘
         Enter open | Space/ filter | s symbol search | q/Esc back
        ");
    }

    #[test]
    fn test_render_file_content_with_line_numbers() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(
                state,
                "src/app.rs",
                "fn main() {\n    println!(\"hi\");\n}\n",
            );
        }
        app.state = AppState::RepoBrowseFile;
        assert_snapshot!(render_at(&mut app, 80, 12), @r#"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┌src/app.rs (1/3)──────────────────────────────────┐
        │▼ src/                    ││    1 fn main() {                                 │
        │    app.rs                ││    2     println!("hi");                         │
        │                          ││    3 }                                           │
        │                          ││                                                  │
        │                          ││                                                  │
        │                          ││                                                  │
        └──────────────────────────┘└──────────────────────────────────────────────────┘
         o outline | s symbol search | gd definition | gf editor | q/Esc back
        "#);
    }

    #[test]
    fn test_line_numbers_are_not_shifted_by_a_literal_hunk_header_in_the_file() {
        let mut app = app_with_browse(&["src/header.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "src/header.rs", "first\n@@ -1,1 +1,1 @@\nthird\n");
        }
        app.state = AppState::RepoBrowseFile;

        assert_snapshot!(render_at(&mut app, 80, 12), @r"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┌src/header.rs (1/3)───────────────────────────────┐
        │▼ src/                    ││    1 first                                       │
        │    header.rs             ││    2 @@ -1,1 +1,1 @@                             │
        │                          ││    3 third                                       │
        │                          ││                                                  │
        │                          ││                                                  │
        │                          ││                                                  │
        └──────────────────────────┘└──────────────────────────────────────────────────┘
         o outline | s symbol search | gd definition | gf editor | q/Esc back
        ");
    }

    #[test]
    fn test_scrolling_deep_into_a_long_file_renders_the_correct_window() {
        let source: String = (1..=30).map(|line| format!("line {line}\n")).collect();
        let mut app = app_with_browse(&["src/long.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "src/long.rs", &source);
            state.cursor_line = 27;
            state.scroll_offset = 25;
        }
        app.state = AppState::RepoBrowseFile;

        assert_snapshot!(render_at(&mut app, 80, 10), @r"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┌src/long.rs (28/30)───────────────────────────────┐
        │▼ src/                    ││   26 line 26                                     ║
        │    long.rs               ││   27 line 27                                     ║
        │                          ││   28 line 28                                     ║
        │                          ││   29 line 29                                     █
        └──────────────────────────┘└──────────────────────────────────────────────────┘
         o outline | s symbol search | gd definition | gf editor | q/Esc back
        ");
    }

    /// Pins the exact viewport rendered by a deep scroll into a huge file.
    ///
    /// This test proves the rendered window, not its cost. The O(viewport) cost
    /// comparison lives in the `browse_render` group in `benches/ui_rendering.rs`.
    #[test]
    fn test_deep_scroll_into_a_huge_file_renders_exactly_the_viewport_rows() {
        const SCROLL: usize = 100;

        let render_scrolled = |file_lines: usize| {
            let mut app = app_with_browse(&["src/huge.rs"]);
            if let Some(state) = app.browse_state.as_mut() {
                open_file(state, "src/huge.rs", &numbered_source(file_lines));
                state.cursor_line = SCROLL;
                state.scroll_offset = SCROLL;
            }
            app.state = AppState::RepoBrowseFile;
            render_at(&mut app, 80, 24)
        };

        let small = render_scrolled(200);
        let huge = render_scrolled(30_000);

        assert!(huge.contains("  101 line 101"), "{huge}");
        assert!(huge.contains("  118 line 118"), "{huge}");
        assert!(!huge.contains("line 119"), "{huge}");
        assert!(huge.contains("(101/30000)"), "{huge}");
        assert!(small.contains("(101/200)"), "{small}");
    }

    /// Proves the content start is found from the header prefix, not by filtering.
    ///
    /// Real pseudo-patches contain headers only as a leading prefix. Poisoning a
    /// mid-file cache entry as a header distinguishes the contiguous prefix/slice
    /// implementation from an O(file) filter: the poisoned line must remain in
    /// the rendered window. `test_line_numbers_are_not_shifted_by_a_literal_hunk_header_in_the_file`
    /// covers the real-input side of the same contract.
    #[test]
    fn test_content_window_finds_the_content_start_by_prefix_not_by_filtering() {
        let mut cache = cache_of(&numbered_source(30));
        // cache.lines[0] is the pseudo-patch header, so cache.lines[i] is file line i.
        cache.lines[15].line_type = LineType::Header;

        let window = content_window(&cache, 0, 40);

        assert_eq!(window.total, 30);
        assert_eq!(window.lines.len(), 30);

        let lines = content_lines(&window, 0, false);
        let content_of = |line: &Line| -> String {
            line.spans[1..]
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert_eq!(content_of(&lines[14]), "line 15");
        assert_eq!(content_of(&lines[29]), "line 30");
    }

    #[test]
    fn test_content_window_borrows_the_viewport_out_of_the_cache() {
        let cache = cache_of(&numbered_source(5_000));

        let window = content_window(&cache, 4_000, 20);

        assert_eq!(window.total, 5_000);
        assert_eq!(window.first_line, 4_000);
        assert_eq!(window.lines.len(), 20);
        // The window is the cache's own storage rather than a copy of it: window
        // line i is cache line i + 1 + first_line (1 = the `@@` header prefix).
        assert!(std::ptr::eq(&window.lines[0], &cache.lines[4_001]));
        assert!(std::ptr::eq(&window.lines[19], &cache.lines[4_020]));
    }

    /// The second half of the same cost story: a frame's spans must point into
    /// the cache's interner instead of copying it. `Span::styled(text.to_string())`
    /// draws the identical frame while allocating once per span per keystroke, so
    /// the `Cow` variant is the only thing that can tell them apart.
    #[test]
    fn test_content_lines_borrow_their_text() {
        let cache = cache_of("fn main() {\n    println!(\"hi\");\n}\n");
        let window = content_window(&cache, 0, 3);

        let lines = content_lines(&window, 0, true);

        assert_eq!(lines.len(), 3);
        for line in &lines {
            // The gutter is generated per frame, so it is the one owned span.
            assert!(
                matches!(line.spans[0].content, Cow::Owned(_)),
                "the line-number gutter should be the only owned span"
            );
            assert!(line.spans.len() > 1, "line rendered without content spans");
            for span in &line.spans[1..] {
                assert!(
                    matches!(span.content, Cow::Borrowed(_)),
                    "span text was copied out of the cache: {:?}",
                    span.content
                );
            }
        }
        // The borrowed spans are the file's own text, marker already stripped.
        let content_of = |line: &Line| -> String {
            line.spans[1..]
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert_eq!(lines[0].spans[0].content.as_ref(), "    1 ");
        assert_eq!(content_of(&lines[0]), "fn main() {");
        assert_eq!(content_of(&lines[2]), "}");
    }

    /// The gutter is the one part of a browse row the renderer writes itself, so
    /// it is the one that can overflow its column. `{:>5}` pads to *at least*
    /// five columns rather than truncating, so line 100,000 silently pushed its
    /// own text one column right of every other line.
    ///
    /// The boundary is exact rather than round: `MAX_VIEWABLE_FILE_LINES` is an
    /// inclusive cap, so a 100,000-line file opens and reaches line 100,000 —
    /// the first six-digit line number — while 99,999 is the last five-digit one.
    #[test]
    fn test_line_number_gutter_at_the_hundred_thousandth_line() {
        // `ilog10` panics on zero, and a window over an empty cache reports
        // `total == 0`. The clamp that keeps that from taking the whole TUI
        // down has no other test.
        assert_eq!(gutter_width(0), LINE_NUMBER_WIDTH);
        assert_eq!(gutter_width(99_999), LINE_NUMBER_WIDTH);
        assert_eq!(gutter_width(100_000), 6);
        assert_eq!(gutter_width(MAX_VIEWABLE_FILE_LINES), 6);
        // The widest a file the browser will open can ever need.
        assert_eq!(gutter_width(MAX_VIEWABLE_FILE_LINES + 1), 6);

        // Render the last three lines of each boundary file and read how wide
        // each row's gutter came out. A gutter that overflows on one line only
        // shifts that line, so the widths stop agreeing.
        let tail_gutters = |total: usize| -> Vec<usize> {
            let cache = cache_of(&numbered_source(total));
            let window = content_window(&cache, total - 3, 3);
            assert_eq!(window.total, total);
            content_lines(&window, total - 1, false)
                .iter()
                .map(|line| line.spans[0].content.chars().count())
                .collect()
        };

        // 99,999 lines: five-column gutter, and it must not widen early.
        assert_eq!(tail_gutters(99_999), vec![6, 6, 6]);
        // 100,000 lines: six columns on every row, including 99,998 and 99,999.
        assert_eq!(tail_gutters(100_000), vec![7, 7, 7]);
    }

    #[test]
    fn test_content_window_clamps_empty_short_and_over_scrolled_files() {
        let empty = cache_of("");
        let window = content_window(&empty, 0, 10);
        assert_eq!(window.total, 0);
        assert_eq!(window.first_line, 0);
        assert!(window.lines.is_empty());

        let three = cache_of("a\nb\nc\n");
        assert_eq!(content_window(&three, 0, 10).lines.len(), 3);
        assert_eq!(content_window(&three, 1, 10).lines.len(), 2);
        assert_eq!(content_window(&three, 1, 10).first_line, 1);

        // Scrolled past the end: an empty window anchored at the end, not a panic.
        let past_end = content_window(&three, 99, 10);
        assert_eq!(past_end.first_line, 3);
        assert!(past_end.lines.is_empty());

        // A pane with no room for content.
        assert!(content_window(&three, 0, 0).lines.is_empty());
    }

    #[test]
    fn test_zen_mode_removes_chrome_and_expands_the_browse_panes() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "src/app.rs", "first\nsecond\nthird\nfourth\n");
        }
        app.state = AppState::RepoBrowseFile;

        let non_zen = render_at(&mut app, 60, 8);
        assert!(non_zen.contains("Repo Browse"), "{non_zen}");
        assert!(non_zen.contains("outline"), "{non_zen}");
        assert!(!non_zen.contains("4 fourth"), "{non_zen}");

        app.zen_mode = true;
        assert_snapshot!(render_at(&mut app, 60, 8), @r"
        ┌Files──────────────┐┌src/app.rs (1/4)─────────────────────┐
        │▼ src/             ││    1 first                          │
        │    app.rs         ││    2 second                         │
        │                   ││    3 third                          │
        │                   ││    4 fourth                         │
        │                   ││                                     │
        │                   ││                                     │
        └───────────────────┘└─────────────────────────────────────┘
        ");
    }

    #[test]
    fn test_render_binary_file_notice() {
        let mut app = app_with_browse(&["blob.bin"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "blob.bin", "");
            if let Some(open) = state.open.as_mut() {
                open.viewable = false;
                open.notice = Some("Binary file — no text preview.".to_string());
            }
        }
        app.state = AppState::RepoBrowseFile;
        let out = render_at(&mut app, 80, 10);
        assert!(out.contains("Binary file"), "{out}");
    }

    #[test]
    fn test_render_listing_error() {
        let mut app = app_with_browse(&[]);
        if let Some(state) = app.browse_state.as_mut() {
            state.paths = LoadState::Error("not a git repository".to_string());
        }
        let out = render_at(&mut app, 80, 10);
        assert!(out.contains("not a git repository"), "{out}");
        assert!(out.contains("unavailable"), "{out}");
    }

    #[test]
    fn test_render_outline_overlay() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(
                state,
                "src/app.rs",
                "struct App;\nimpl App {\n  fn run() {}\n}\n",
            );
            if let Some(open) = state.open.as_mut() {
                open.symbols = vec![
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
                ];
            }
            state.overlay = BrowseOverlay::Outline { selected: 1 };
        }
        app.state = AppState::RepoBrowseFile;
        let out = render_at(&mut app, 80, 14);
        assert!(out.contains("Outline (2 symbols)"), "{out}");
        assert!(out.contains("C App"), "{out}");
        assert!(out.contains("m run"), "{out}");
    }

    #[test]
    fn test_render_symbol_search_overlay() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            state.index = IndexState::Ready(Arc::new(SymbolIndex::from_files(vec![FileSymbols {
                path: "src/app.rs".to_string(),
                symbols: vec![Symbol {
                    name: "render_app".to_string(),
                    kind: SymbolKind::Function,
                    line: 42,
                    column: 3,
                    depth: 0,
                }],
            }])));
            state.overlay = BrowseOverlay::SymbolSearch {
                query: "app".to_string(),
                selected: 0,
            };
        }
        let out = render_at(&mut app, 80, 16);
        assert!(out.contains("Symbol search"), "{out}");
        assert!(out.contains("1 matches"), "{out}");
        assert!(out.contains("render_app"), "{out}");
        assert!(out.contains("src/app.rs:42"), "{out}");
    }

    /// The text of the one row the symbol-search overlay paints as selected.
    fn highlighted_row(buffer: &Buffer, height: u16) -> Option<String> {
        (0..height).find_map(|y| {
            let row: String = (0..buffer.area.width)
                .filter(|x| buffer[(*x, y)].bg == Color::Cyan)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            (!row.trim().is_empty()).then(|| row.trim().to_string())
        })
    }

    #[test]
    fn test_symbol_search_render_and_jump_share_one_result_set() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            state.index = IndexState::Ready(Arc::new(SymbolIndex::from_files(vec![FileSymbols {
                path: "src/app.rs".to_string(),
                symbols: (0..MAX_SYMBOL_SEARCH_RESULTS + 50)
                    .map(|n| Symbol {
                        name: format!("sym_{n:04}"),
                        kind: SymbolKind::Function,
                        line: n + 1,
                        column: 0,
                        depth: 0,
                    })
                    .collect(),
            }])));
            state.overlay = BrowseOverlay::SymbolSearch {
                query: "sym".to_string(),
                selected: 0,
            };
        }

        // More symbols match than the cap admits, so a renderer that applied a
        // different cap would announce a count Enter cannot index into.
        let reachable = app
            .browse_state
            .as_ref()
            .unwrap()
            .symbol_search_results("sym")
            .len();
        assert_eq!(reachable, MAX_SYMBOL_SEARCH_RESULTS);

        let out = render_at(&mut app, 80, 16);
        assert!(
            out.contains(&format!("{reachable} matches")),
            "the rendered result count must be the count Enter indexes into; {out}"
        );
    }

    #[test]
    fn test_symbol_search_enter_opens_the_row_the_renderer_highlights() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "src/app.rs", &numbered_source(60));
            state.index = IndexState::Ready(Arc::new(SymbolIndex::from_files(vec![FileSymbols {
                path: "src/app.rs".to_string(),
                symbols: (0..6)
                    .map(|n| Symbol {
                        name: format!("sym_{n}"),
                        kind: SymbolKind::Function,
                        line: 7 * n + 3,
                        column: 0,
                        depth: 0,
                    })
                    .collect(),
            }])));
            state.overlay = BrowseOverlay::SymbolSearch {
                query: "sym".to_string(),
                selected: 0,
            };
        }
        for _ in 0..3 {
            app.handle_repo_browse_tree_input(press(KeyCode::Down))
                .unwrap();
        }

        let row = highlighted_row(&render_buffer(&mut app, 80, 20), 20)
            .expect("the overlay must paint the selected row");
        let (path, line) = row
            .rsplit_once(' ')
            .and_then(|(_, location)| location.rsplit_once(':'))
            .expect("a result row ends in path:line");
        let line: usize = line.parse().unwrap();

        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.open.as_ref().unwrap().path, path);
        assert_eq!(state.cursor_line, line - 1);
    }

    #[test]
    fn test_filter_input_resumes_after_symbol_search_overlay_closes() {
        let mut app =
            app_with_browse(&["src/alpha.rs", "src/beta.rs", "tests/alpha.rs", "README.md"]);
        if let Some(state) = app.browse_state.as_mut() {
            state.index = IndexState::Ready(Arc::new(SymbolIndex::from_files(vec![FileSymbols {
                path: "src/beta.rs".to_string(),
                symbols: vec![Symbol {
                    name: "beta".to_string(),
                    kind: SymbolKind::Function,
                    line: 7,
                    column: 0,
                    depth: 0,
                }],
            }])));
            state.filter = Some(ListFilter::new());
        }

        app.handle_repo_browse_tree_input(press(KeyCode::Char('a')))
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.filter.as_ref().unwrap().query, "a");
        let filter_render = render_at(&mut app, 120, 20);
        assert!(filter_render.contains("Filter: a_"), "{filter_render}");

        if let Some(state) = app.browse_state.as_mut() {
            // The active filter consumes every character before shared
            // keybindings, so real keys cannot create this combination. Build
            // it directly to preserve the overlay-before-filter precedence contract.
            state.overlay = BrowseOverlay::SymbolSearch {
                query: "b".to_string(),
                selected: 0,
            };
        }
        app.handle_repo_browse_tree_input(press(KeyCode::Char('e')))
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert_eq!(state.filter.as_ref().unwrap().query, "a");
        assert!(matches!(
            state.overlay,
            BrowseOverlay::SymbolSearch {
                ref query,
                selected: 0
            } if query == "be"
        ));

        let overlay_render = render_at(&mut app, 120, 20);
        assert!(overlay_render.contains("Filter: a_"), "{overlay_render}");
        assert!(overlay_render.contains("Symbol search"), "{overlay_render}");
        assert!(overlay_render.contains("1 matches"), "{overlay_render}");

        app.handle_repo_browse_tree_input(press(KeyCode::Esc))
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.overlay, BrowseOverlay::None));
        assert_eq!(state.filter.as_ref().unwrap().query, "a");
        assert!(state.filter.as_ref().unwrap().input_active);

        app.handle_repo_browse_tree_input(press(KeyCode::Char('c')))
            .unwrap();
        assert_eq!(
            app.browse_state
                .as_ref()
                .unwrap()
                .filter
                .as_ref()
                .unwrap()
                .query,
            "ac"
        );
    }

    #[test]
    fn test_committed_filter_survives_reachable_symbol_search_flow() {
        let mut app =
            app_with_browse(&["src/alpha.rs", "src/beta.rs", "tests/alpha.rs", "README.md"]);
        if let Some(state) = app.browse_state.as_mut() {
            state.index = IndexState::Ready(Arc::new(SymbolIndex::from_files(vec![FileSymbols {
                path: "src/beta.rs".to_string(),
                symbols: vec![Symbol {
                    name: "beta".to_string(),
                    kind: SymbolKind::Function,
                    line: 7,
                    column: 0,
                    depth: 0,
                }],
            }])));
        }

        let filter_keys = app.config.keybindings.filter.keys.clone();
        for binding in filter_keys {
            app.handle_repo_browse_tree_input(press_binding(binding))
                .unwrap();
        }
        for character in "alpha".chars() {
            app.handle_repo_browse_tree_input(press(KeyCode::Char(character)))
                .unwrap();
        }
        app.handle_repo_browse_tree_input(press(KeyCode::Enter))
            .unwrap();

        let state = app.browse_state.as_ref().unwrap();
        let filter = state.filter.as_ref().unwrap();
        assert_eq!(filter.query, "alpha");
        assert!(!filter.input_active);
        assert_eq!(filter.matched_indices, vec![0, 2]);
        assert!(state.tree.find_row_for_file(0).is_some());
        assert!(state.tree.find_row_for_file(1).is_none());
        assert!(state.tree.find_row_for_file(2).is_some());

        let symbol_search_binding = app
            .config
            .keybindings
            .symbol_search
            .all_sequences()
            .find_map(|sequence| match sequence {
                [binding] => Some(*binding),
                _ => None,
            })
            .expect("symbol_search must have a single-key binding");
        app.handle_repo_browse_tree_input(press_binding(symbol_search_binding))
            .unwrap();
        assert!(matches!(
            app.browse_state.as_ref().unwrap().overlay,
            BrowseOverlay::SymbolSearch {
                ref query,
                selected: 0
            } if query.is_empty()
        ));

        for character in "be".chars() {
            app.handle_repo_browse_tree_input(press(KeyCode::Char(character)))
                .unwrap();
        }
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(
            state.overlay,
            BrowseOverlay::SymbolSearch {
                ref query,
                selected: 0
            } if query == "be"
        ));
        assert_eq!(state.filter.as_ref().unwrap().query, "alpha");

        app.handle_repo_browse_tree_input(press(KeyCode::Esc))
            .unwrap();
        let state = app.browse_state.as_ref().unwrap();
        assert!(matches!(state.overlay, BrowseOverlay::None));
        let filter = state.filter.as_ref().unwrap();
        assert_eq!(filter.query, "alpha");
        assert!(!filter.input_active);
        assert_eq!(filter.matched_indices, vec![0, 2]);
        assert!(state.tree.find_row_for_file(0).is_some());
        assert!(state.tree.find_row_for_file(1).is_none());
        assert!(state.tree.find_row_for_file(2).is_some());

        let rendered = render_at(&mut app, 120, 20);
        assert!(rendered.contains("Files (/alpha)"), "{rendered}");
        assert!(!rendered.contains("Filter: alpha_"), "{rendered}");
        assert!(!rendered.contains("Symbol search"), "{rendered}");
    }

    #[test]
    fn test_cjk_content_uses_terminal_cell_width_for_alignment() {
        let mut ascii_app = app_with_browse(&["src/width.rs"]);
        if let Some(state) = ascii_app.browse_state.as_mut() {
            open_file(state, "src/width.rs", "abcdefgh|\n");
        }
        ascii_app.state = AppState::RepoBrowseFile;
        let ascii = render_buffer(&mut ascii_app, 80, 10);

        let mut cjk_app = app_with_browse(&["src/width.rs"]);
        if let Some(state) = cjk_app.browse_state.as_mut() {
            open_file(state, "src/width.rs", "日本語あ|\n");
        }
        cjk_app.state = AppState::RepoBrowseFile;
        let cjk = render_buffer(&mut cjk_app, 80, 10);

        let content_y = 4;
        let ascii_sentinel_x = (0..80)
            .find(|&x| ascii[(x, content_y)].symbol() == "|")
            .unwrap();
        let cjk_sentinel_x = (0..80)
            .find(|&x| cjk[(x, content_y)].symbol() == "|")
            .unwrap();
        assert_eq!(ascii_sentinel_x, cjk_sentinel_x);

        let wide_char_x = (0..80)
            .find(|&x| cjk[(x, content_y)].symbol() == "日")
            .unwrap();
        assert_eq!(cjk[(wide_char_x + 1, content_y)], Cell::EMPTY);

        let ascii_right_border_x = (0..80)
            .rev()
            .find(|&x| ascii[(x, content_y)].symbol() == "│")
            .unwrap();
        let cjk_right_border_x = (0..80)
            .rev()
            .find(|&x| cjk[(x, content_y)].symbol() == "│")
            .unwrap();
        assert_eq!(ascii_right_border_x, cjk_right_border_x);
    }

    /// A CJK line wider than the pane has to be cut somewhere. The cut must land
    /// on a character boundary: half a double-width glyph would either bleed over
    /// the pane border or leave the border painted inside the glyph's cell.
    /// Both alignments matter — with an odd-width prefix the glyph that no longer
    /// fits straddles the border, which is the case that goes wrong.
    #[test]
    fn test_cjk_line_wider_than_the_pane_is_cut_on_a_character_boundary() {
        let wide_only = "あ".repeat(40);
        let odd_offset = format!("x{}", "あ".repeat(40));

        for source in [wide_only, odd_offset] {
            let mut app = app_with_browse(&["src/width.rs"]);
            if let Some(state) = app.browse_state.as_mut() {
                open_file(state, "src/width.rs", &format!("{source}\n"));
            }
            app.state = AppState::RepoBrowseFile;
            let buf = render_buffer(&mut app, 80, 10);

            let content_y = 4;
            let border_x = 79;
            assert_eq!(
                buf[(border_x, content_y)].symbol(),
                "│",
                "the pane border was overwritten by the overflowing line: {source}"
            );

            let wide_positions: Vec<u16> = (0..border_x)
                .filter(|&x| buf[(x, content_y)].symbol() == "あ")
                .collect();
            // The line really does overflow the ~44-cell text column.
            assert!(
                wide_positions.len() >= 20,
                "expected an overflowing line, drew {} glyphs: {source}",
                wide_positions.len()
            );
            for x in wide_positions {
                assert!(
                    x + 1 < border_x,
                    "a double-width glyph at x={x} runs into the border"
                );
                assert_eq!(
                    buf[(x + 1, content_y)],
                    Cell::EMPTY,
                    "the cell after the glyph at x={x} is not its continuation"
                );
            }
        }
    }

    /// A CJK glyph in the tree can start in the column immediately left of the
    /// overlay, putting its second half exactly on the overlay's left border.
    /// [`Clear`] resets only the cells inside the overlay, so the glyph survives
    /// and ratatui's buffer diff — which skips the cell after a double-width
    /// symbol — never emits the border at all.
    ///
    /// A text snapshot cannot see this: a continuation cell holds a space either
    /// way, so the rendered text is identical whether the frame survives or not.
    /// The assertions therefore read cells.
    #[test]
    fn test_overlay_left_border_survives_a_wide_glyph_straddling_it() {
        // Tree rows start their name in column 3, so twelve leading ASCII
        // columns put the first wide glyph on columns 15-16 — and the 60%-wide
        // overlay's left border lands on column 16 of an 80-column frame.
        let paths: Vec<String> = (0..8).map(|i| format!("{i:0>12}日本語")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut app = app_with_browse(&refs);

        let area = overlay_rect(Rect::new(0, 0, 80, 14), 60, 70);
        assert_eq!((area.x, area.width), (16, 48));

        // The fixture really does straddle the border: without the overlay the
        // wide glyph occupies columns 15-16 on every tree row it covers.
        let bare = render_buffer(&mut app, 80, 14);
        let straddling_rows: Vec<u16> = (area.top()..area.bottom())
            .filter(|&y| bare[(15, y)].symbol() == "日")
            .collect();
        assert!(
            straddling_rows.len() >= 5,
            "fixture drew {} straddling rows, too few to exercise the border",
            straddling_rows.len()
        );

        if let Some(state) = app.browse_state.as_mut() {
            state.overlay = BrowseOverlay::Outline { selected: 0 };
        }
        let overlaid = render_buffer(&mut app, 80, 14);

        for y in area.top()..area.bottom() {
            let left = overlaid[(area.x - 1, y)].symbol();
            assert!(
                left.width() <= 1,
                "row {y}: {left:?} left of the overlay bleeds over its left border"
            );
        }
        for &y in &straddling_rows {
            let border = overlaid[(area.x, y)].symbol();
            assert!(
                matches!(border, "│" | "┌" | "└"),
                "row {y}: the overlay's left border is {border:?}"
            );
        }
    }

    /// The same repair, on the other overlay.
    ///
    /// `clear_overlay_area` has two call sites and the sibling test above only
    /// covers the outline's. The symbol-search overlay is wider, so its left
    /// border lands on a different column and needs its own fixture — swapping
    /// its call for a bare `Clear` is invisible to every other test.
    #[test]
    fn test_symbol_search_left_border_survives_a_wide_glyph_straddling_it() {
        // The 80%-wide overlay's left border lands on column 8 of an 80-column
        // frame, and tree rows start their name in column 3 — so four leading
        // ASCII columns put the first wide glyph on columns 7-8.
        let paths: Vec<String> = (0..8).map(|i| format!("{i:0>4}日本語")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let mut app = app_with_browse(&refs);

        let area = overlay_rect(Rect::new(0, 0, 80, 14), 80, 70);
        assert_eq!((area.x, area.width), (8, 64));

        let bare = render_buffer(&mut app, 80, 14);
        let straddling_rows: Vec<u16> = (area.top()..area.bottom())
            .filter(|&y| bare[(7, y)].symbol() == "日")
            .collect();
        assert!(
            straddling_rows.len() >= 5,
            "fixture drew {} straddling rows, too few to exercise the border",
            straddling_rows.len()
        );

        if let Some(state) = app.browse_state.as_mut() {
            state.index = IndexState::Ready(Arc::new(SymbolIndex::from_files(Vec::new())));
            state.overlay = BrowseOverlay::SymbolSearch {
                query: String::new(),
                selected: 0,
            };
        }
        let overlaid = render_buffer(&mut app, 80, 14);

        for y in area.top()..area.bottom() {
            let left = overlaid[(area.x - 1, y)].symbol();
            assert!(
                left.width() <= 1,
                "row {y}: {left:?} left of the overlay bleeds over its left border"
            );
        }
        for &y in &straddling_rows {
            let border = overlaid[(area.x, y)].symbol();
            assert!(
                matches!(border, "│" | "┌" | "└"),
                "row {y}: the overlay's left border is {border:?}"
            );
        }
    }

    /// The other edge needs no repair, and this pins the ratatui behaviour that
    /// makes that true: the overlay's right border overwrites the first half of
    /// a glyph that started in the overlay's last column, and the continuation
    /// cell it leaves behind already holds a space rather than an empty symbol,
    /// so the border and the cell after it both render normally. If ratatui ever
    /// represented continuation cells differently, the right edge would need the
    /// same treatment as the left and this test would say so.
    #[test]
    fn test_overlay_right_edge_needs_no_repair_because_continuations_are_spaces() {
        // The content pane's text starts in column 35 of an 80-column frame, so
        // 28 ASCII columns put the first wide glyph on columns 63-64, and 63 is
        // the overlay's right border.
        let source = format!("{}日本語\n", "a".repeat(28));
        let mut app = app_with_browse(&["src/width.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "src/width.rs", &source);
        }
        app.state = AppState::RepoBrowseFile;

        let area = overlay_rect(Rect::new(0, 0, 80, 14), 60, 70);
        let border_x = area.right() - 1;
        let outside_x = area.right();
        let content_y = 4;

        let bare = render_buffer(&mut app, 80, 14);
        assert_eq!(bare[(border_x, content_y)].symbol(), "日");
        assert_eq!(
            bare[(outside_x, content_y)].symbol(),
            " ",
            "a continuation cell is no longer a space; the right edge now needs repairing too"
        );

        if let Some(state) = app.browse_state.as_mut() {
            state.overlay = BrowseOverlay::Outline { selected: 0 };
        }
        let overlaid = render_buffer(&mut app, 80, 14);

        assert_eq!(
            overlaid[(border_x, content_y)].symbol(),
            "│",
            "the overlay's right border is missing"
        );
        assert_eq!(overlaid[(outside_x, content_y)].symbol(), " ");
    }

    #[test]
    fn test_render_status_message_replaces_hints() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            state.status = Some("No definition found".to_string());
        }
        let out = render_at(&mut app, 80, 10);
        assert!(out.contains("No definition found"), "{out}");
    }

    #[test]
    fn test_symbol_search_overlay_is_reviewably_clipped_in_a_tiny_terminal() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "src/app.rs", "fn main() {}\n");
            state.overlay = BrowseOverlay::SymbolSearch {
                query: String::new(),
                selected: 0,
            };
        }
        app.state = AppState::RepoBrowseFile;
        assert_snapshot!(render_at(&mut app, 20, 5), @r"
        ┌Symbol search─────┐
        │  _               │
        └──────────────────┘
        ┌Type to search 0 s┐
        └ ↑/↓ or Ctrl-p/n m┘
        ");
    }

    /// Terminals too small for the chrome clip it away pane by pane, and the
    /// frame stays complete and deterministic at every step. The 12x6 case is the
    /// one with teeth: the content pane still has borders but no room for a single
    /// line, so the viewport window is empty while the file is not — snapshotting
    /// it pins that the pane draws its frame instead of drawing nothing (or
    /// panicking on the empty slice).
    #[test]
    fn test_render_in_degenerate_terminals_clips_instead_of_panicking() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "src/app.rs", "fn main() {}\n");
        }
        app.state = AppState::RepoBrowseFile;

        assert_snapshot!(render_at(&mut app, 12, 6), @r"
        ┌──────────┐
        │Repo Brows│
        └──────────┘
        ┌Fi┐┌src/ap┐
        └──┘└──────┘
         o outline |
        ");

        // No room for the body at all: header top border and footer survive.
        assert_snapshot!(render_at(&mut app, 4, 2), @r"
        ┌──┐
         o o
        ");

        // A single cell.
        assert_snapshot!(render_at(&mut app, 1, 1), @"");

        // The same sizes with an overlay stacked on top of the clipped panes.
        if let Some(state) = app.browse_state.as_mut() {
            state.overlay = BrowseOverlay::SymbolSearch {
                query: String::new(),
                selected: 0,
            };
        }
        assert_snapshot!(render_at(&mut app, 4, 2), @r"
        ┌Sy┐
        └──┘
        ");
        assert_snapshot!(render_at(&mut app, 1, 1), @"┌");
    }

    // ===== pure helpers =====

    #[test]
    fn test_scroll_offset_centres_selection() {
        assert_eq!(scroll_offset(0, 100, 10), 0);
        assert_eq!(scroll_offset(50, 100, 10), 45);
        // Never scrolls past the end.
        assert_eq!(scroll_offset(99, 100, 10), 90);
    }

    #[test]
    fn test_scroll_offset_when_everything_fits() {
        assert_eq!(scroll_offset(3, 5, 10), 0);
        assert_eq!(scroll_offset(3, 5, 0), 0);
    }

    #[test]
    fn test_overlay_rect_is_centred_and_bounded() {
        let area = Rect::new(0, 0, 100, 40);
        let rect = overlay_rect(area, 80, 50);
        assert_eq!((rect.width, rect.height), (80, 20));
        assert_eq!((rect.x, rect.y), (10, 10));

        // A terminal smaller than the minimum still yields a valid rect.
        let tiny = overlay_rect(Rect::new(0, 0, 10, 4), 80, 70);
        assert!(tiny.width <= 10 && tiny.height <= 4);
    }
}
