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

use crate::app::browse::{BrowseOverlay, BrowseState};
use crate::app::{App, AppState, LoadState, TreeRow};
use crate::diff::LineType;
use crate::symbols::Symbol;

/// Width of the gutter holding line numbers.
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

    // The cache carries a leading hunk header from the pseudo-patch; content
    // lines start right after it, so line N of the file is cache line N+1.
    let content: Vec<_> = open
        .cache
        .lines
        .iter()
        .filter(|line| line.line_type != LineType::Header)
        .collect();

    let total = content.len();
    let start = state.scroll_offset.min(total);
    let end = (start + inner_height).min(total);

    let lines: Vec<Line<'_>> = content[start..end]
        .iter()
        .enumerate()
        .map(|(offset, cached)| {
            let line_index = start + offset;
            let is_cursor = line_index == state.cursor_line;

            let mut spans = Vec::with_capacity(cached.spans.len() + 1);
            spans.push(Span::styled(
                format!("{:>width$} ", line_index + 1, width = LINE_NUMBER_WIDTH),
                Style::default().fg(if is_cursor {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
            ));

            for (index, span) in cached.spans.iter().enumerate() {
                let text = open.cache.resolve(span.content);
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
                spans.push(Span::styled(text.to_string(), style));
            }
            Line::from(spans)
        })
        .collect();

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

fn render_outline(frame: &mut Frame, state: &BrowseState, selected: usize) {
    let area = overlay_rect(frame.area(), 60, 70);
    frame.render_widget(Clear, area);

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
    frame.render_widget(Clear, area);

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

    let results = state.symbol_search_results(query);
    let inner_height = rows[1].height.saturating_sub(2) as usize;
    let offset = scroll_offset(selected, results.len(), inner_height);

    let items: Vec<ListItem> = results
        .iter()
        .enumerate()
        .skip(offset)
        .take(inner_height)
        .map(|(index, (_, _, label))| {
            let style = if index == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(format!(" {label}"), style)))
        })
        .collect();

    let title = if query.is_empty() {
        format!(
            "Type to search {} symbols",
            state.index.ready().map_or(0, |i| i.symbol_count())
        )
    } else {
        format!("{} matches", results.len())
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
    use crate::app::browse::{build_file_patch, BrowseState, IndexState, OpenFile};
    use crate::config::Config;
    use crate::symbols::{FileSymbols, Symbol, SymbolIndex, SymbolKind};
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn render_at(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buf = terminal.backend().buffer();
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
    fn test_render_in_a_tiny_terminal_does_not_panic() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "src/app.rs", "fn main() {}\n");
            state.overlay = BrowseOverlay::SymbolSearch {
                query: String::new(),
                selected: 0,
            };
        }
        app.state = AppState::RepoBrowseFile;
        let _ = render_at(&mut app, 20, 5);
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
