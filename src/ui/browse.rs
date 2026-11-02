//! Repository Browser rendering: file tree, file content, and overlays.

use std::collections::HashSet;

use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState,
    },
    Frame,
};

use unicode_width::UnicodeWidthStr;

use crate::app::browse::{
    BlameCoverage, BlameGutter, BlameGutterWidth, BlameState, BrowseCommitDiffState, BrowseOverlay,
    BrowseState, ModuleGraphDirection, ModuleGraphPaneState, ModuleGraphPanel, ModuleGraphRow,
    PrLookupState, BLAME_AUTHOR_WIDTH, BLAME_FULL_WIDTH, BLAME_IDENTITY_WIDTH,
};
use crate::app::browse_discussion::{
    DiscussionIndex, DiscussionView, LineDiscussionFailure, LineDiscussionState,
};
use crate::app::{App, AppState, CachedDiffLine, DiffCache, LoadState, TreeRow};
use crate::diff::LineType;
use crate::github::{CommitPullRequest, CommitPullRequestState};
use crate::symbols::Symbol;
use crate::ui::chrome::{pane_block, PaneTitle};
use crate::ui::common::truncate_with_width;
use crate::ui::hit::{HitMap, HitTarget, ListKind, PaneKind, TabGroup};
use crate::ui::theme::{Attention, Theme, SELECTION_MARKER_WIDTH};

/// Narrowest the line-number gutter ever gets.
///
/// Five columns hold every line number up to 99,999. Beyond that the gutter
/// grows rather than overflowing — see [`gutter_width`].
const LINE_NUMBER_WIDTH: usize = 5;
const MIN_CODE_WIDTH_WITH_BLAME: usize = 24;
const DISCUSSION_GUTTER_WIDTH: usize = 2;
const CURSOR_LINE_BG: Color = Color::Rgb(48, 48, 64);

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

    let theme = app.theme;
    let attention = app.browse_attention();
    let graph_visible = app
        .browse_state
        .as_ref()
        .is_some_and(|state| state.module_graph_pane.is_visible());
    let left_width = app.config.layout.left_panel_width;
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(left_width),
            Constraint::Percentage(100 - left_width),
        ])
        .split(body);

    render_tree(frame, app, columns[0], attention.tree, theme);
    if graph_visible {
        let right = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(columns[1]);
        render_content(frame, app, right[0], attention.file, theme);
        if let Some(state) = app.browse_state.as_ref() {
            let kb = &app.config.keybindings;
            let graph_help = if attention.graph.has_keys() {
                format!(
                    " Tab/{}/{} dir | {}/{} node | {} open | {} close | Esc/{} code ",
                    kb.move_left.display_primary(),
                    kb.move_right.display_primary(),
                    kb.move_down.display_primary(),
                    kb.move_up.display_primary(),
                    kb.open_panel.display_primary(),
                    kb.module_graph.display_primary(),
                    kb.quit.display_primary(),
                )
            } else if app.state == AppState::RepoBrowseFile {
                format!(" {}/→ focus ", kb.module_graph.display_primary())
            } else {
                String::new()
            };
            render_module_graph_pane(frame, state, right[1], attention.graph, theme, &graph_help);
        }
        register_module_graph_hits(app, right[1]);
    } else {
        render_content(frame, app, columns[1], attention.file, theme);
    }

    super::split_view::register_split_divider(app, columns[0], body);

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

    let theme = app.theme;
    let index_style = match state.index {
        crate::app::browse::IndexState::Ready(_) => theme.success.style(),
        crate::app::browse::IndexState::Failed { .. } => theme.danger.style(),
        crate::app::browse::IndexState::Building | crate::app::browse::IndexState::Idle => {
            theme.info.style()
        }
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled("Repo Browse", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" - "),
        Span::styled(root, theme.accent.style()),
        Span::raw("  "),
        Span::styled(file_count, theme.muted.style()),
        Span::raw("  "),
        Span::styled(state.index.label(), index_style),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}

fn render_tree(frame: &mut Frame, app: &mut App, area: Rect, attention: Attention, theme: Theme) {
    let Some(state) = app.browse_state.as_ref() else {
        return;
    };

    let files_block = |detail: &str| {
        pane_block(
            theme,
            PaneTitle::Labeled {
                label: "Files",
                detail,
            },
            attention,
        )
    };

    if let LoadState::Error(ref message) = state.paths {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            format!("  {message}"),
            Style::default().fg(Color::Red),
        )))
        .block(files_block(""));
        frame.render_widget(paragraph, area);
        return;
    }

    if matches!(state.paths, LoadState::Loading | LoadState::NotLoaded) {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            format!("  {} Listing files…", app.spinner_char()),
            Style::default().fg(Color::DarkGray),
        )))
        .block(files_block(""));
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

            let mark = attention.rows(theme);
            let name_width = (area.width as usize)
                .saturating_sub(2)
                .saturating_sub(SELECTION_MARKER_WIDTH);
            let shown = truncate_with_width(&text, name_width);
            ListItem::new(Line::from(vec![
                mark.prefix(selected),
                Span::styled(shown.into_owned(), base.patch(mark.body_style(selected))),
            ]))
        })
        .collect();

    let block = match state.filter {
        Some(ref filter) if filter.input_active => {
            let detail = format!("Filter: {}_", filter.query);
            pane_block(
                theme,
                PaneTitle::Labeled {
                    label: "",
                    detail: &detail,
                },
                attention,
            )
        }
        Some(ref filter) if !filter.query.is_empty() => {
            let detail = format!("Files (/{})", filter.query);
            pane_block(
                theme,
                PaneTitle::Labeled {
                    label: "",
                    detail: &detail,
                },
                attention,
            )
        }
        _ => files_block(""),
    };
    let inner_area = block.inner(area);
    let list = List::new(items).block(block);
    frame.render_widget(list, area);

    app.hit_map.push(
        inner_area,
        HitTarget::Pane {
            pane: PaneKind::List(ListKind::BrowseTree),
        },
    );
    for (i, _) in state
        .tree
        .visible_rows
        .iter()
        .skip(offset)
        .take(inner_height)
        .enumerate()
    {
        let row = offset + i;
        app.hit_map.push(
            Rect {
                x: inner_area.x,
                y: inner_area.y + i as u16,
                width: inner_area.width,
                height: 1,
            },
            HitTarget::ListRow {
                list: ListKind::BrowseTree,
                row,
                index: row,
            },
        );
    }
}

fn render_content(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    attention: Attention,
    theme: Theme,
) {
    let bg_color = app.config.diff.bg_color;
    let spinner = app.spinner_char().to_string();
    let Some(state) = app.browse_state.as_mut() else {
        return;
    };

    if state.commit_diff.is_active() {
        render_commit_diff(frame, state, area, attention, theme, bg_color, &spinner);
        return;
    }

    let inner_area = area.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    app.hit_map.push(
        inner_area,
        HitTarget::Pane {
            pane: PaneKind::BrowseFile,
        },
    );

    let Some(open) = state.open.as_ref() else {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            "  Select a file to view it.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(pane_block(
            theme,
            PaneTitle::Labeled {
                label: "Preview",
                detail: "",
            },
            attention,
        ));
        frame.render_widget(paragraph, area);
        return;
    };

    if let Some(ref notice) = open.notice {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            format!("  {notice}"),
            Style::default().fg(Color::DarkGray),
        )))
        .block(pane_block(
            theme,
            PaneTitle::Labeled {
                label: "",
                detail: &open.path,
            },
            attention,
        ));
        frame.render_widget(paragraph, area);
        return;
    }

    let inner_height = area.height.saturating_sub(2) as usize;
    state.clamp_scroll(inner_height);

    let state = app.browse_state.as_ref().expect("browse state");
    let open = state.open.as_ref().expect("open file");

    let window = content_window(&open.cache, state.scroll_offset, inner_height);
    let total = window.total;
    let blame = match state.blame {
        BlameState::Ready {
            ref path,
            ref gutter,
        } if path == &open.path => Some(gutter),
        _ => None,
    };
    let discussion = match state.line_discussion {
        LineDiscussionState::Ready { ref index, .. } => Some(index),
        _ => None,
    };
    let content_width = area.width.saturating_sub(2) as usize;
    let (lines, logical_lines) = content_lines_with_logical_lines(
        &window,
        state.cursor_line,
        bg_color,
        blame,
        discussion,
        content_width,
        inner_height,
    );

    let building = matches!(state.index, crate::app::browse::IndexState::Building);
    let title = if open.cache.highlighted || !building {
        format!("{} ({}/{})", open.path, state.cursor_line + 1, total.max(1))
    } else {
        format!("{} {}", open.path, spinner)
    };

    let paragraph = Paragraph::new(lines).block(pane_block(
        theme,
        PaneTitle::Labeled {
            label: "",
            detail: &title,
        },
        attention,
    ));
    frame.render_widget(paragraph, area);

    for (rendered_row, line) in logical_lines.into_iter().enumerate() {
        app.hit_map.push(
            Rect {
                x: inner_area.x,
                y: inner_area.y.saturating_add(rendered_row as u16),
                width: inner_area.width,
                height: 1,
            },
            HitTarget::ContentLine {
                pane: PaneKind::BrowseFile,
                line,
            },
        );
    }

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

fn render_commit_diff(
    frame: &mut Frame,
    state: &mut BrowseState,
    area: Rect,
    attention: Attention,
    theme: Theme,
    bg_color: bool,
    spinner: &str,
) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let (title, lines, adjusted_scroll, scrollbar) = match &mut state.commit_diff {
        BrowseCommitDiffState::Off => return,
        BrowseCommitDiffState::Loading { annotation, .. } => (
            format!(
                "{} {} — {}",
                annotation.short_sha(),
                annotation.summary(),
                annotation.author_name()
            ),
            vec![Line::from(Span::styled(
                format!("  {spinner} Loading commit diff…"),
                Style::default().fg(Color::Yellow),
            ))],
            0,
            None,
        ),
        BrowseCommitDiffState::Failed {
            annotation,
            message,
        } => (
            format!(
                "{} {} — {}",
                annotation.short_sha(),
                annotation.summary(),
                annotation.author_name()
            ),
            vec![Line::from(Span::styled(
                format!("  Commit diff unavailable: {message}"),
                Style::default().fg(Color::Red),
            ))],
            0,
            None,
        ),
        BrowseCommitDiffState::Ready {
            annotation,
            cache,
            scroll,
        } => {
            scroll.set_visible_lines(inner_height);
            let title = format!(
                "{} {} — {}",
                annotation.short_sha(),
                annotation.summary(),
                annotation.author_name()
            );
            if cache.lines.is_empty() {
                (
                    title,
                    vec![Line::from(Span::styled(
                        "  This commit has no diff.",
                        Style::default().fg(Color::DarkGray),
                    ))],
                    0,
                    None,
                )
            } else {
                // Margin mode can push the offset past the last page; clamp
                // like the diff view does so no blank rows render.
                let max_scroll = cache.lines.len().saturating_sub(inner_height);
                let offset = scroll.scroll_offset.min(max_scroll);
                let visible_start = offset.saturating_sub(2).min(cache.lines.len());
                let visible_end = (offset + inner_height + 5).min(cache.lines.len());
                let lines = crate::ui::diff_view::render_cached_lines(
                    cache,
                    visible_start..visible_end,
                    scroll.selected_line,
                    &HashSet::new(),
                    bg_color,
                    None,
                    area.width.saturating_sub(2),
                );
                let adjusted_scroll = offset.saturating_sub(visible_start) as u16;
                (
                    title,
                    lines,
                    adjusted_scroll,
                    (max_scroll > 0).then_some((max_scroll, offset)),
                )
            }
        }
    };

    let paragraph = Paragraph::new(lines)
        .block(pane_block(
            theme,
            PaneTitle::Labeled {
                label: "",
                detail: &title,
            },
            attention,
        ))
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((adjusted_scroll, 0));
    frame.render_widget(paragraph, area);

    if let Some((max_scroll, position)) = scrollbar {
        let mut scrollbar_state = ScrollbarState::new(max_scroll).position(position);
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
///
/// A file line wider than the pane continues on extra visual rows instead of
/// being cut off; `max_rows` caps the emitted rows at the viewport height so
/// the work stays O(viewport) even when every visible line wraps.
#[cfg(test)]
fn content_lines<'a>(
    window: &ContentWindow<'a>,
    cursor_line: usize,
    bg_color: bool,
    blame: Option<&'a BlameGutter>,
    discussion: Option<&DiscussionIndex>,
    content_width: usize,
    max_rows: usize,
) -> Vec<Line<'a>> {
    content_lines_with_logical_lines(
        window,
        cursor_line,
        bg_color,
        blame,
        discussion,
        content_width,
        max_rows,
    )
    .0
}

fn content_lines_with_logical_lines<'a>(
    window: &ContentWindow<'a>,
    cursor_line: usize,
    bg_color: bool,
    blame: Option<&'a BlameGutter>,
    discussion: Option<&DiscussionIndex>,
    content_width: usize,
    max_rows: usize,
) -> (Vec<Line<'a>>, Vec<usize>) {
    let width = gutter_width(window.total);
    let discussion_width = discussion.map_or(0, |_| DISCUSSION_GUTTER_WIDTH);
    let blame_width =
        blame_gutter_width(content_width.saturating_sub(discussion_width), window.total);

    let mut out = WrappedRows::default();
    for (offset, cached) in window.lines.iter().enumerate() {
        if out.len() >= max_rows {
            break;
        }
        let line_index = window.first_line + offset;
        let is_cursor = line_index == cursor_line;

        let mut prefix = Vec::with_capacity(3);
        if let (Some(gutter), Some(blame_width)) = (blame, blame_width) {
            prefix.push(Span::styled(
                gutter.text(line_index, blame_width),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(index) = discussion {
            prefix.push(Span::styled(
                if index.thread_indices_at(line_index).is_empty() {
                    "  "
                } else {
                    "● "
                },
                Style::default().fg(Color::Cyan),
            ));
        }
        prefix.push(Span::styled(
            format!("{:>width$} ", line_index + 1),
            Style::default().fg(if is_cursor {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ));
        let content: Vec<(&'a str, Style)> = cached
            .spans
            .iter()
            .enumerate()
            .map(|(index, span)| {
                let text = window.cache.resolve(span.content);
                // Strip the pseudo-patch's leading context marker.
                let text = if index == 0 {
                    text.strip_prefix(' ').unwrap_or(text)
                } else {
                    text
                };
                let mut style = span.style;
                if is_cursor && bg_color {
                    style = style.bg(CURSOR_LINE_BG);
                }
                (text, style)
            })
            .collect();

        push_wrapped_line(
            &mut out,
            line_index,
            prefix,
            &content,
            content_width,
            max_rows,
            is_cursor && bg_color,
        );
    }
    (out.rows, out.logical_lines)
}

/// 折返し描画の出力バッファ。可視行とその論理行番号を常に同数で保持する。
#[derive(Default)]
struct WrappedRows<'a> {
    rows: Vec<Line<'a>>,
    logical_lines: Vec<usize>,
}

impl<'a> WrappedRows<'a> {
    fn push(&mut self, logical_line: usize, row: Line<'a>) {
        self.rows.push(row);
        self.logical_lines.push(logical_line);
    }

    fn len(&self) -> usize {
        self.rows.len()
    }
}

/// Append one file line as visual rows no wider than `content_width` cells.
///
/// Rows break on character boundaries: CJK prose has no spaces, so ratatui's
/// word wrapper would push a whole paragraph onto its own row below a lonely
/// line number. Continuation rows carry a blank prefix as wide as the gutter,
/// keeping the line-number column straight. Sub-spans borrow slices of the
/// same interned text as the unwrapped path, and emission stops at `max_rows`.
fn push_wrapped_line<'a>(
    out: &mut WrappedRows<'a>,
    line_index: usize,
    prefix: Vec<Span<'a>>,
    content: &[(&'a str, Style)],
    content_width: usize,
    max_rows: usize,
    cursor_bg: bool,
) {
    let prefix_width: usize = prefix.iter().map(|span| span.content.width()).sum();
    let text_width = content_width.saturating_sub(prefix_width);
    // Degenerate pane: no room for text next to the gutter. Emit the single
    // row the unwrapped path produced and let the renderer clip it.
    if text_width == 0 {
        let mut spans = prefix;
        spans.extend(
            content
                .iter()
                .map(|&(text, style)| Span::styled(text, style)),
        );
        out.push(line_index, Line::from(spans));
        return;
    }

    let pad = || {
        Span::styled(
            " ".repeat(prefix_width),
            if cursor_bg {
                Style::default().bg(CURSOR_LINE_BG)
            } else {
                Style::default()
            },
        )
    };

    let mut current = prefix;
    let mut used = 0usize;
    for &(text, style) in content {
        // Fast path: the whole span fits on this row as one borrowed piece.
        let span_width = text.width();
        if used + span_width <= text_width {
            if !text.is_empty() {
                current.push(Span::styled(text, style));
            }
            used += span_width;
            continue;
        }

        let mut start = 0usize;
        for (byte, ch) in text.char_indices() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            // A glyph wider than the whole text column still gets placed
            // (`used > 0` fails), so every iteration makes progress.
            if used + ch_width > text_width && used > 0 {
                if byte > start {
                    current.push(Span::styled(&text[start..byte], style));
                }
                out.push(line_index, Line::from(std::mem::take(&mut current)));
                if out.len() >= max_rows {
                    return;
                }
                current.push(pad());
                start = byte;
                used = 0;
            }
            used += ch_width;
        }
        if start < text.len() {
            current.push(Span::styled(&text[start..], style));
        }
    }
    out.push(line_index, Line::from(current));
}

fn blame_gutter_width(content_width: usize, total: usize) -> Option<BlameGutterWidth> {
    let available =
        content_width.saturating_sub(gutter_width(total) + 1 + MIN_CODE_WIDTH_WITH_BLAME);
    if available >= BLAME_FULL_WIDTH {
        Some(BlameGutterWidth::Full)
    } else if available >= BLAME_AUTHOR_WIDTH {
        Some(BlameGutterWidth::Author)
    } else if available >= BLAME_IDENTITY_WIDTH {
        Some(BlameGutterWidth::Identity)
    } else {
        None
    }
}

fn fit_footer_line(text: &str, max_width: usize) -> std::borrow::Cow<'_, str> {
    if max_width == 0 {
        std::borrow::Cow::Borrowed("")
    } else {
        truncate_with_width(text, max_width)
    }
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let kb = &app.config.keybindings;
    let state = app.browse_state.as_ref();
    let status = state.and_then(|state| state.status.as_deref());
    let commit_diff = state.map(|state| &state.commit_diff);
    let pr_lookup = state.map(|state| &state.pr_lookup);
    let line_discussion = state.map(|state| &state.line_discussion);
    let coverage = if app.state == AppState::RepoBrowseFile {
        state.and_then(|state| match (&state.open, &state.blame) {
            (Some(open), BlameState::Ready { path, gutter }) if open.path == *path => {
                Some(gutter.coverage())
            }
            _ => None,
        })
    } else {
        None
    };

    let lookup_text = match pr_lookup {
        Some(PrLookupState::Loading { sha, .. }) => Some(format!(
            " {} Looking up pull requests for {}…",
            app.spinner_char(),
            crate::github::short_sha(sha)
        )),
        Some(PrLookupState::Selecting { .. }) => {
            Some(" j/k move | Enter open PR | Esc cancel".to_string())
        }
        Some(PrLookupState::Idle | PrLookupState::Failed { .. }) | None => None,
    };

    let discussion_text = match line_discussion {
        Some(LineDiscussionState::ResolvingPullRequests { .. }) => Some(format!(
            " {} Looking up pull requests for this file…",
            app.spinner_char()
        )),
        Some(LineDiscussionState::LoadingComments { .. }) => Some(format!(
            " {} Loading review discussions…",
            app.spinner_char()
        )),
        Some(LineDiscussionState::Ready {
            view: DiscussionView::ThreadList { .. },
            ..
        }) => Some(" j/k move | Enter expand thread | Esc close".to_string()),
        Some(LineDiscussionState::Ready {
            view: DiscussionView::Expanded { .. },
            ..
        }) => Some(" j/k move | Esc back to threads".to_string()),
        Some(
            LineDiscussionState::Idle
            | LineDiscussionState::Ready {
                view: DiscussionView::Closed,
                ..
            },
        )
        | None => None,
        Some(LineDiscussionState::Failed { failure }) => match failure {
            LineDiscussionFailure::NoPullRequest
            | LineDiscussionFailure::Api
            | LineDiscussionFailure::Anchor => None,
        },
    };

    let text: std::borrow::Cow<'_, str> = if let Some(text) = discussion_text {
        std::borrow::Cow::Owned(text)
    } else if let Some(text) = lookup_text {
        std::borrow::Cow::Owned(text)
    } else {
        match commit_diff {
        Some(BrowseCommitDiffState::Loading { .. }) => std::borrow::Cow::Owned(format!(
            " {} Loading commit diff… | {} back",
            app.spinner_char(),
            kb.quit.display()
        )),
        Some(BrowseCommitDiffState::Ready { .. }) => std::borrow::Cow::Owned(format!(
            " {}/{} scroll | {}/{} page | {}/{} first/last | {} back",
            kb.move_down.display(),
            kb.move_up.display(),
            kb.page_down.display(),
            kb.page_up.display(),
            kb.jump_to_first.display(),
            kb.jump_to_last.display(),
            kb.quit.display(),
        )),
        Some(BrowseCommitDiffState::Failed { message, .. }) => std::borrow::Cow::Owned(format!(
            " Commit diff unavailable: {message} | {} back",
            kb.quit.display()
        )),
        Some(BrowseCommitDiffState::Off) | None => match status {
        Some(message) => fit_footer_line(message, usize::from(area.width)),
        None if app.state == AppState::RepoBrowseTree => std::borrow::Cow::Owned(format!(
            " {} open | {} filter | {} symbol search | {} back",
            kb.open_panel.display(),
            kb.filter.display(),
            kb.symbol_search.display(),
            kb.quit.display(),
        )),
        None if app.state == AppState::RepoBrowseGraph => std::borrow::Cow::Owned(format!(
            " Tab/{}/{} direction | {}/{} node | {} open | {} close graph | Esc/{} code",
            kb.move_left.display_primary(),
            kb.move_right.display_primary(),
            kb.move_down.display_primary(),
            kb.move_up.display_primary(),
            kb.open_panel.display_primary(),
            kb.module_graph.display_primary(),
            kb.quit.display_primary(),
        )),
        None => match coverage {
            Some(
                BlameCoverage::ShorterThanBuffer {
                    blame_lines,
                    buffer_lines,
                }
                | BlameCoverage::LongerThanBuffer {
                    blame_lines,
                    buffer_lines,
                },
            ) => std::borrow::Cow::Owned(format!(
                " blame covers {blame_lines} lines, this file shows {buffer_lines} — reopen the file to refresh"
            )),
            Some(BlameCoverage::Exact) | None => std::borrow::Cow::Owned(format!(
                " {} outline | {} search | {} graph | {} blame | {} diff | {} PR | {} discuss | {} def | {} edit | {} back",
                kb.symbol_outline.display(),
                kb.symbol_search.display(),
                kb.module_graph.display(),
                kb.toggle_blame.display(),
                kb.open_blame_commit.display(),
                kb.open_blame_pr.display(),
                kb.open_line_discussion.display(),
                kb.go_to_definition.display(),
                kb.go_to_file.display(),
                kb.quit.display(),
            )),
        },
        },
        }
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

fn render_overlay(frame: &mut Frame, app: &mut App) {
    let theme = app.theme;
    let frame_area = frame.area();
    let (browse_state, hit_map) = (&mut app.browse_state, &mut app.hit_map);
    let Some(state) = browse_state.as_mut() else {
        return;
    };

    match &mut state.line_discussion {
        LineDiscussionState::Ready { index, view, .. }
            if !matches!(view, DiscussionView::Closed) =>
        {
            let area = overlay_rect(frame_area, 80, 75);
            hit_map.push(frame_area, HitTarget::Backdrop { dismiss: true });
            hit_map.push(area, HitTarget::OverlaySurface);
            render_line_discussion(frame, hit_map, area, index, view, theme);
            return;
        }
        _ => {}
    }

    if let PrLookupState::Selecting {
        pulls, selected, ..
    } = &state.pr_lookup
    {
        let area = overlay_rect(frame_area, 70, 60);
        hit_map.push(frame_area, HitTarget::Backdrop { dismiss: true });
        hit_map.push(area, HitTarget::OverlaySurface);
        render_pr_selection(frame, hit_map, area, pulls, *selected, theme);
        return;
    }

    match state.overlay {
        BrowseOverlay::None => {}
        BrowseOverlay::Outline { selected } => {
            let area = overlay_rect(frame_area, 60, 70);
            hit_map.push(frame_area, HitTarget::Backdrop { dismiss: true });
            hit_map.push(area, HitTarget::OverlaySurface);
            render_outline(frame, hit_map, area, state, selected, theme);
        }
        BrowseOverlay::SymbolSearch {
            ref query,
            selected,
        } => {
            let area = overlay_rect(frame_area, 80, 70);
            hit_map.push(frame_area, HitTarget::Backdrop { dismiss: true });
            hit_map.push(area, HitTarget::OverlaySurface);
            render_symbol_search(frame, hit_map, area, state, query, selected, theme);
        }
    }
}

fn render_line_discussion(
    frame: &mut Frame,
    hit_map: &mut HitMap,
    area: Rect,
    index: &DiscussionIndex,
    view: &mut DiscussionView,
    theme: Theme,
) {
    clear_overlay_area(frame, area);
    let content_area = if let Some(note) = index.confidence_note() {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("⚠ Incomplete: {note}"),
                Style::default().fg(Color::Yellow),
            ))),
            rows[0],
        );
        rows[1]
    } else {
        area
    };
    let register_rows = |hit_map: &mut HitMap, item_heights: &[usize], offset: usize| {
        let inner = Block::default().borders(Borders::ALL).inner(content_area);
        let bottom = inner.y.saturating_add(inner.height);
        let mut y = inner.y;
        for (row, &item_height) in item_heights.iter().enumerate().skip(offset) {
            if y >= bottom {
                break;
            }
            let remaining_height = usize::from(bottom - y);
            if item_height > remaining_height {
                break;
            }
            hit_map.push(
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: item_height as u16,
                },
                HitTarget::ListRow {
                    list: ListKind::BrowseLineDiscussion,
                    row,
                    index: row,
                },
            );
            y = y.saturating_add(item_height as u16);
        }
    };
    let resolved = HashSet::new();
    let outdated_ids = HashSet::new();
    match view {
        DiscussionView::Closed => {}
        DiscussionView::ThreadList {
            line,
            selected,
            scroll,
        } => {
            let body_width = (content_area.width.saturating_sub(4) as usize).saturating_sub(4);
            let visible_threads = index.thread_indices_at(*line);
            let item_heights = visible_threads
                .iter()
                .filter_map(|thread_index| index.threads.get(*thread_index))
                .map(|thread| {
                    let comment = &index.comments[thread.root];
                    2 + comment
                        .body
                        .lines()
                        .map(|line| crate::ui::common::wrap_text(line, body_width).len())
                        .sum::<usize>()
                })
                .collect::<Vec<_>>();
            crate::ui::comment_list::render_review_thread_list_data(
                frame,
                content_area,
                crate::ui::comment_list::ReviewThreadListData {
                    comments: &index.comments,
                    threads: &index.threads,
                    visible_threads: Some(visible_threads),
                    selected: *selected,
                    resolved_ids: &resolved,
                    outdated_ids: &outdated_ids,
                },
                scroll,
                theme,
                Attention::Focused,
            );
            register_rows(hit_map, &item_heights, *scroll);
        }
        DiscussionView::Expanded {
            line,
            thread_position,
            selected,
            scroll,
        } => {
            let Some(thread_index) = index.thread_indices_at(*line).get(*thread_position) else {
                return;
            };
            let Some(thread) = index.threads.get(*thread_index) else {
                return;
            };
            let body_width = (content_area.width.saturating_sub(4) as usize).saturating_sub(6);
            let item_heights = std::iter::once(thread.root)
                .chain(thread.replies.iter().copied())
                .map(|comment_index| {
                    let comment = &index.comments[comment_index];
                    2 + comment
                        .body
                        .lines()
                        .map(|line| crate::ui::common::wrap_text(line, body_width).len())
                        .sum::<usize>()
                })
                .collect::<Vec<_>>();
            crate::ui::comment_list::render_review_thread_data(
                frame,
                content_area,
                &index.comments,
                thread,
                *selected,
                scroll,
                &resolved,
                &outdated_ids,
                theme,
                Attention::Focused,
            );
            register_rows(hit_map, &item_heights, *scroll);
        }
    }
}

fn render_pr_selection(
    frame: &mut Frame,
    hit_map: &mut HitMap,
    area: Rect,
    pulls: &[CommitPullRequest],
    selected: usize,
    theme: Theme,
) {
    clear_overlay_area(frame, area);
    let inner_height = area.height.saturating_sub(2) as usize;
    let offset = scroll_offset(selected, pulls.len(), inner_height);
    let items = pulls
        .iter()
        .enumerate()
        .skip(offset)
        .take(inner_height)
        .map(|(_, pull)| {
            let state = match pull.state {
                CommitPullRequestState::Open => "open",
                CommitPullRequestState::Closed => "closed",
                CommitPullRequestState::Merged => "merged",
                CommitPullRequestState::Unknown => "unconfirmed",
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("#{} ", pull.number), theme.muted.style()),
                Span::raw(format!("[{state}] {}", pull.title)),
            ]))
        })
        .collect::<Vec<_>>();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(
            "Pull requests containing this commit ({})",
            pulls.len()
        ))
        .title_bottom(Line::from(Span::styled(
            " j/k move | Enter open | Esc cancel ",
            Style::default().fg(Color::DarkGray),
        )));
    let inner_area = block.inner(area);
    let visible_selected = selected.saturating_sub(offset);
    let mut list_state = ListState::default().with_selected(Some(visible_selected));
    let list = Attention::Focused
        .rows(theme)
        .apply(List::new(items).block(block));
    frame.render_stateful_widget(list, area, &mut list_state);
    for visible_row in 0..pulls.len().saturating_sub(offset).min(inner_height) {
        let row = offset + visible_row;
        hit_map.push(
            Rect {
                x: inner_area.x,
                y: inner_area.y.saturating_add(visible_row as u16),
                width: inner_area.width,
                height: 1,
            },
            HitTarget::ListRow {
                list: ListKind::BrowsePrChoice,
                row,
                index: row,
            },
        );
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

fn render_outline(
    frame: &mut Frame,
    hit_map: &mut HitMap,
    area: Rect,
    state: &BrowseState,
    selected: usize,
    theme: Theme,
) {
    clear_overlay_area(frame, area);

    let inner_height = area.height.saturating_sub(2) as usize;
    let symbols = state.outline_symbols();
    let offset = scroll_offset(selected, symbols.len(), inner_height);

    let items: Vec<ListItem> = symbols
        .iter()
        .enumerate()
        .skip(offset)
        .take(inner_height)
        .map(|(_, symbol)| ListItem::new(outline_row(symbol)))
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!("Outline ({} symbols)", symbols.len()))
        .title_bottom(Line::from(Span::styled(
            " j/k move | Enter jump | Esc close ",
            Style::default().fg(Color::DarkGray),
        )));
    let inner_area = block.inner(area);
    let visible_selected = selected.saturating_sub(offset);
    let mut list_state = ListState::default().with_selected(Some(visible_selected));
    let list = Attention::Focused
        .rows(theme)
        .apply(List::new(items).block(block));
    frame.render_stateful_widget(list, area, &mut list_state);
    for visible_row in 0..symbols.len().saturating_sub(offset).min(inner_height) {
        let row = offset + visible_row;
        hit_map.push(
            Rect {
                x: inner_area.x,
                y: inner_area.y.saturating_add(visible_row as u16),
                width: inner_area.width,
                height: 1,
            },
            HitTarget::ListRow {
                list: ListKind::BrowseOutline,
                row,
                index: row,
            },
        );
    }
}

fn render_module_graph_pane(
    frame: &mut Frame,
    state: &BrowseState,
    area: Rect,
    attention: Attention,
    theme: Theme,
    graph_help: &str,
) {
    match &state.module_graph_pane {
        ModuleGraphPaneState::Closed => {}
        ModuleGraphPaneState::Waiting { path } => {
            render_module_graph_loading_pane(
                frame,
                area,
                path,
                " Waiting for the current file…",
                module_graph_block("", "Graph (opening…)", attention, theme, graph_help),
            );
        }
        ModuleGraphPaneState::Loading { path, .. } => {
            render_module_graph_loading_pane(
                frame,
                area,
                path,
                " Resolving imports and importers…",
                module_graph_block("Graph", "(loading…)", attention, theme, graph_help),
            );
        }
        ModuleGraphPaneState::Ready(panel) => {
            render_module_graph_ready_pane(frame, area, panel, attention, theme, graph_help);
        }
    }
}

fn render_module_graph_loading_pane(
    frame: &mut Frame,
    area: Rect,
    path: &str,
    message: &str,
    block: Block,
) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let mut lines = module_graph_box_lines("current module", path, inner_width, true)
        .into_iter()
        .map(|text| {
            Line::from(Span::styled(
                text,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(Span::styled(
        message,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_module_graph_ready_pane(
    frame: &mut Frame,
    area: Rect,
    panel: &ModuleGraphPanel,
    attention: Attention,
    theme: Theme,
    graph_help: &str,
) {
    let current = panel.current();
    let title = module_graph_title(panel, usize::from(area.width.saturating_sub(2)));
    let block = module_graph_block("", &title, attention, theme, graph_help);
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;
    let current_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let mut lines =
        module_graph_box_lines("current module", &panel.display_path, inner_width, true)
            .into_iter()
            .map(|text| Line::from(Span::styled(text, current_style)))
            .collect::<Vec<_>>();

    if current.rows.is_empty() {
        let empty = match panel.direction {
            ModuleGraphDirection::Dependencies => " No imports.",
            ModuleGraphDirection::Dependents => " No importers.",
        };
        lines.push(Line::from(Span::styled(
            empty,
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let visible_nodes = inner_height.saturating_sub(3) / 3;
        let offset = scroll_offset(panel.selected, current.rows.len(), visible_nodes);
        for (index, row) in current
            .rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible_nodes)
        {
            let style = if index == panel.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if row.jump.is_none() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let last = index + 1 == current.rows.len();
            for text in module_graph_relationship_lines(row, panel.direction, inner_width, last) {
                lines.push(Line::from(Span::styled(text, style)));
            }
        }
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn module_graph_title(panel: &ModuleGraphPanel, title_width: usize) -> String {
    let current = panel.current();
    let direction = match panel.direction {
        ModuleGraphDirection::Dependencies => "IMPORTS →",
        ModuleGraphDirection::Dependents => "IMPORTERS",
    };
    let guarantee = match current.guarantee {
        crate::module_graph::DependencyGuarantee::Exact => "exact",
        crate::module_graph::DependencyGuarantee::Approximate => "approximate",
    };
    let count = if current.rows.len() < current.total {
        format!("{}/{}", current.rows.len(), current.total)
    } else {
        current.total.to_string()
    };
    let full = format!("{direction} {count} {guarantee}");
    if format!("▶ {full}").width() <= title_width {
        return full;
    }
    match current.guarantee {
        crate::module_graph::DependencyGuarantee::Exact => format!("{direction} {count}"),
        crate::module_graph::DependencyGuarantee::Approximate => format!("{direction} ~{count}"),
    }
}

/// モジュールグラフペインのヒット領域を登録する。
/// レンダラーは &BrowseState 借用中で app に触れないため、同じ
/// scroll_offset / 1 ノード 3 行のレイアウト計算をここで共有して逆写像する。
fn register_module_graph_hits(app: &mut App, area: Rect) {
    let Some(state) = app.browse_state.as_ref() else {
        return;
    };
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let mut regions: Vec<(Rect, HitTarget)> = vec![
        (
            inner,
            HitTarget::Pane {
                pane: PaneKind::ModuleGraph,
            },
        ),
        // 上枠(タイトル行)クリックで方向トグル
        (
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
            HitTarget::Tab {
                group: TabGroup::GraphDirection,
                index: 0,
            },
        ),
    ];

    if let ModuleGraphPaneState::Ready(panel) = &state.module_graph_pane {
        let rows = panel.current_rows();
        let inner_height = usize::from(inner.height);
        let visible_nodes = inner_height.saturating_sub(3) / 3;
        let offset = scroll_offset(panel.selected, rows.len(), visible_nodes);
        for (visible_index, index) in (offset..rows.len().min(offset + visible_nodes)).enumerate() {
            let y = inner.y.saturating_add(3 + (visible_index * 3) as u16);
            if y >= inner.bottom() {
                break;
            }
            regions.push((
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 3.min(inner.bottom().saturating_sub(y)),
                },
                HitTarget::ListRow {
                    list: ListKind::BrowseGraph,
                    row: index,
                    index,
                },
            ));
        }
    }

    for (rect, target) in regions {
        app.hit_map.push(rect, target);
    }
}

fn module_graph_block<'a>(
    label: &'a str,
    detail: &'a str,
    attention: Attention,
    theme: Theme,
    graph_help: &str,
) -> Block<'a> {
    pane_block(theme, PaneTitle::Labeled { label, detail }, attention).title_bottom(Line::from(
        Span::styled(graph_help.to_string(), theme.muted.style()),
    ))
}

fn module_graph_relationship_lines(
    row: &ModuleGraphRow,
    direction: ModuleGraphDirection,
    width: usize,
    last: bool,
) -> [String; 3] {
    let prefix_width = usize::from(width >= 7) * 3;
    let box_width = width.saturating_sub(prefix_width);
    let [top, middle, bottom] = module_graph_box_lines(&row.node, &row.edge, box_width, false);
    if prefix_width == 0 {
        return [top, middle, bottom];
    }
    let connector = match (direction, last) {
        (ModuleGraphDirection::Dependencies, false) => "├─▶",
        (ModuleGraphDirection::Dependencies, true) => "└─▶",
        (ModuleGraphDirection::Dependents, false) => "▲─┤",
        (ModuleGraphDirection::Dependents, true) => "▲─┘",
    };
    let tail = if last { "   " } else { "│  " };
    [
        format!("│  {top}"),
        format!("{connector}{middle}"),
        format!("{tail}{bottom}"),
    ]
}

fn module_graph_box_lines(
    title: &str,
    detail: &str,
    width: usize,
    emphasized: bool,
) -> [String; 3] {
    if width < 4 {
        return [
            fit_module_graph_text(title, width),
            fit_module_graph_text(detail, width),
            fit_module_graph_text("─", width),
        ];
    }

    let (top_left, horizontal, top_right, vertical, bottom_left, bottom_right) = if emphasized {
        ('╔', '═', '╗', '║', '╚', '╝')
    } else {
        ('┌', '─', '┐', '│', '└', '┘')
    };
    let inner = width - 2;
    let top = if inner <= 3 {
        format!(
            "{top_left}{}{top_right}",
            horizontal.to_string().repeat(inner)
        )
    } else {
        let title = truncate_with_width(title, inner - 3);
        let title_width = title.width();
        let fill = inner.saturating_sub(title_width + 3);
        format!(
            "{top_left}{horizontal} {title} {}{top_right}",
            horizontal.to_string().repeat(fill)
        )
    };
    let middle = format!(
        "{vertical}{}{vertical}",
        fit_module_graph_text(detail, inner)
    );
    let bottom = format!(
        "{bottom_left}{}{bottom_right}",
        horizontal.to_string().repeat(inner)
    );
    [top, middle, bottom]
}

fn fit_module_graph_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let text = truncate_with_width(text, width);
    let padding = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(padding))
}

fn outline_row(symbol: &Symbol) -> Line<'static> {
    let style = Style::default();
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

fn render_symbol_search(
    frame: &mut Frame,
    hit_map: &mut HitMap,
    area: Rect,
    state: &BrowseState,
    query: &str,
    selected: usize,
    theme: Theme,
) {
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
        .map(|(_, hit)| {
            let label = hit.search_label();
            ListItem::new(Line::from(Span::raw(label)))
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title)
        .title_bottom(Line::from(Span::styled(
            " ↑/↓ or Ctrl-p/n move | Enter jump | Esc close ",
            Style::default().fg(Color::DarkGray),
        )));
    let inner_area = block.inner(rows[1]);
    let visible_selected = selected.saturating_sub(offset);
    let mut list_state = ListState::default().with_selected(Some(visible_selected));
    let list = Attention::Focused
        .rows(theme)
        .apply(List::new(items).block(block));
    frame.render_stateful_widget(list, rows[1], &mut list_state);
    for visible_row in 0..hits.len().saturating_sub(offset).min(inner_height) {
        let row = offset + visible_row;
        hit_map.push(
            Rect {
                x: inner_area.x,
                y: inner_area.y.saturating_add(visible_row as u16),
                width: inner_area.width,
                height: 1,
            },
            HitTarget::ListRow {
                list: ListKind::BrowseSymbolSearch,
                row,
                index: row,
            },
        );
    }
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
        build_file_patch, BlameAnnotation, BrowseCommitDiffState, BrowseState, IndexState,
        ModuleGraphDirection, ModuleGraphPanel, ModuleGraphRow, ModuleGraphRows, OpenFile,
        PrLookupState, MAX_SYMBOL_SEARCH_RESULTS, MAX_VIEWABLE_FILE_LINES,
    };
    use crate::app::browse_discussion::{
        DiscussionIndex, DiscussionOutcome, DiscussionView, LineDiscussionFailure,
        LineDiscussionState,
    };
    use crate::app::CommentThread;
    use crate::config::Config;
    use crate::diff_store::{DiffScrollState, ScrollMode};
    use crate::filter::ListFilter;
    use crate::github::{parse_porcelain, CommitPullRequest, CommitPullRequestState};
    use crate::keybinding::{KeyBinding, KeySequence};
    use crate::module_graph::DependencyGuarantee;
    use crate::symbols::{FileSymbols, Symbol, SymbolIndex, SymbolKind};
    use crate::ui::theme::{Attention, Theme};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};
    use ratatui::Terminal;
    use std::borrow::Cow;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn discussion_index(line_count: usize, marked_line: usize) -> DiscussionIndex {
        let comment = serde_json::from_value(serde_json::json!({
            "id": 1,
            "path": "main.rs",
            "line": 2,
            "body": "Discuss this line",
            "user": { "login": "reviewer" },
            "created_at": "2026-07-28T00:00:00Z"
        }))
        .unwrap();
        let mut line_threads = vec![smallvec::SmallVec::new(); line_count];
        line_threads[marked_line].push(0);
        DiscussionIndex {
            comments: vec![comment],
            threads: vec![CommentThread {
                root: 0,
                replies: vec![],
            }],
            line_threads,
            file_thread_count: 1,
            comment_paths: vec!["main.rs".to_string()],
            outcome: DiscussionOutcome::Complete,
        }
    }

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

    #[test]
    fn fit_footer_line_truncates_fullwidth_text_within_the_footer_width() {
        let samples = [
            "あいうえお",
            "読み取り拒否: src/日本語.rs",
            "Symbol index is unavailable: 読み取り拒否: src/日本語.rs",
            "aあb",
        ];
        assert!(
            !samples[0].is_char_boundary(1),
            "fullwidth text must not be sliced on a byte boundary"
        );
        for text in samples {
            for width in 0..=text.width() + 1 {
                let fitted = fit_footer_line(text, width);
                assert!(
                    fitted.width() <= width,
                    "footer {width} held {} columns: {fitted}",
                    fitted.width()
                );
                if width == 0 {
                    assert_eq!(fitted, "");
                } else if text.width() > width {
                    assert!(fitted.ends_with('…'), "{fitted}");
                    let kept = fitted.trim_end_matches('…');
                    assert!(text.starts_with(kept), "{fitted}");
                } else {
                    assert_eq!(fitted, text);
                }
            }
        }
    }

    #[test]
    fn narrow_footer_ends_a_fullwidth_status_with_an_ellipsis() {
        let mut app = app_with_browse(&["src/日本語.rs"]);
        app.browse_state.as_mut().unwrap().status =
            Some("Symbol index is unavailable: 読み取り拒否: src/日本語.rs".to_string());
        let rendered = render_at(&mut app, 16, 8);
        let last = rendered.lines().next_back().unwrap_or("");
        assert!(last.ends_with('…'), "{rendered}");
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

    fn attach_blame(state: &mut BrowseState, path: &str, porcelain: &str) {
        let buffer_lines = state.open.as_ref().map_or(0, OpenFile::line_count);
        state.blame = BlameState::Ready {
            path: path.to_string(),
            gutter: BlameGutter::from_file(parse_porcelain(porcelain), buffer_lines),
        };
    }

    fn blame_annotation(porcelain: &str) -> Arc<BlameAnnotation> {
        let gutter = BlameGutter::from_file(parse_porcelain(porcelain), 1);
        Arc::clone(gutter.annotation_at(0).expect("blame annotation"))
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
    fn focused_open_file_title_keeps_the_path() {
        let mut app = app_with_browse(&["src/app.rs"]);
        open_file(
            app.browse_state.as_mut().unwrap(),
            "src/app.rs",
            "fn main() {}\n",
        );
        app.state = AppState::RepoBrowseFile;
        let text = render_at(&mut app, 100, 16);
        assert!(
            text.contains("▶ src/app.rs (1/"),
            "path title missing:\n{text}"
        );
        assert!(!text.contains("SRC/APP.RS"), "path was uppercased:\n{text}");
    }

    #[test]
    fn test_render_tree_and_empty_preview() {
        let mut app = app_with_browse(&["src/app.rs", "src/ui/mod.rs", "README.md"]);
        assert_snapshot!(render_at(&mut app, 80, 14), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  3 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┏▶ FILES━━━━━━━━━━━━━━━━━━━┓┌Preview───────────────────────────────────────────┐
        ┃▌ ▼ src/                  ┃│  Select a file to view it.                       │
        ┃    ▼ ui/                 ┃│                                                  │
        ┃        mod.rs            ┃│                                                  │
        ┃      app.rs              ┃│                                                  │
        ┃    README.md             ┃│                                                  │
        ┃                          ┃│                                                  │
        ┃                          ┃│                                                  │
        ┃                          ┃│                                                  │
        ┗━━━━━━━━━━━━━━━━━━━━━━━━━━┛└──────────────────────────────────────────────────┘
         Enter open | Space/ filter | s symbol search | q/Esc back
        ");
    }

    #[test]
    fn test_multiple_commit_pull_requests_render_a_stateful_selection_popup() {
        let mut app = app_with_browse(&["src/app.rs"]);
        app.state = AppState::RepoBrowseFile;
        app.browse_state.as_mut().unwrap().pr_lookup = PrLookupState::Selecting {
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            pulls: vec![
                CommitPullRequest {
                    number: 10,
                    title: "still open".to_string(),
                    state: CommitPullRequestState::Open,
                },
                CommitPullRequest {
                    number: 20,
                    title: "already closed".to_string(),
                    state: CommitPullRequestState::Closed,
                },
                CommitPullRequest {
                    number: 30,
                    title: "already merged".to_string(),
                    state: CommitPullRequestState::Merged,
                },
            ],
            selected: 1,
        };

        assert_snapshot!(render_at(&mut app, 100, 16), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────┌Pull requests containing this commit (3)────────────────────────────┐──────────────┐
        │▏ ▼ src/      │  #10 [open] still open                                             │              │
        │      app.rs  │▌ #20 [closed] already closed                                       │              │
        │              │  #30 [merged] already merged                                       │              │
        │              │                                                                    │              │
        │              │                                                                    │              │
        │              │                                                                    │              │
        │              │                                                                    │              │
        │              └ j/k move | Enter open | Esc cancel ────────────────────────────────┘              │
        │                                 ││                                                               │
        │                                 ││                                                               │
        └─────────────────────────────────┘└───────────────────────────────────────────────────────────────┘
         j/k move | Enter open PR | Esc cancel
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
        ┌Files─────────────────────┐┏▶ src/app.rs (1/3)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏ ▼ src/                  │┃    1 fn main() {                                 ┃
        │      app.rs              │┃    2     println!("hi");                         ┃
        │                          │┃    3 }                                           ┃
        │                          │┃                                                  ┃
        │                          │┃                                                  ┃
        │                          │┃                                                  ┃
        └──────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         o outline | s search | i graph | gb blame | gc diff | gp PR | gr discuss | gd d
        "#);
    }

    #[test]
    fn test_commit_diff_loading_ready_empty_and_failed_render_inside_the_browser() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        const DIFF: &str = "diff --git a/main.rs b/main.rs\n\
             index 1111111..2222222 100644\n\
             --- a/main.rs\n\
             +++ b/main.rs\n\
             @@ -1 +1 @@\n\
             -fn old() {}\n\
             +fn new() {}\n";

        let annotation = blame_annotation(PORCELAIN);
        let mut renders = Vec::new();

        let mut loading = app_with_browse(&["main.rs"]);
        if let Some(state) = loading.browse_state.as_mut() {
            open_file(state, "main.rs", "fn new() {}\n");
            state.commit_diff = BrowseCommitDiffState::Loading {
                request_id: 1,
                annotation: Arc::clone(&annotation),
                cancel: state.cancel_token.child_token(),
            };
        }
        loading.state = AppState::RepoBrowseFile;
        renders.push(format!(
            "--- loading ---\n{}",
            render_at(&mut loading, 100, 10)
        ));

        let mut ready = app_with_browse(&["main.rs"]);
        if let Some(state) = ready.browse_state.as_mut() {
            open_file(state, "main.rs", "fn new() {}\n");
            let mut pool = crate::syntax::ParserPool::new();
            let cache = crate::ui::diff_view::build_commit_diff_cache(
                DIFF,
                "base16-ocean.dark",
                &mut pool,
                4,
            );
            let mut scroll = DiffScrollState::new(ScrollMode::Margin);
            scroll.set_line_count(cache.lines.len());
            state.commit_diff = BrowseCommitDiffState::Ready {
                annotation: Arc::clone(&annotation),
                cache,
                scroll,
            };
        }
        ready.state = AppState::RepoBrowseFile;
        renders.push(format!("--- ready ---\n{}", render_at(&mut ready, 100, 12)));

        let mut empty = app_with_browse(&["main.rs"]);
        if let Some(state) = empty.browse_state.as_mut() {
            open_file(state, "main.rs", "fn new() {}\n");
            let mut pool = crate::syntax::ParserPool::new();
            let cache = crate::ui::diff_view::build_commit_diff_cache(
                "",
                "base16-ocean.dark",
                &mut pool,
                4,
            );
            let mut scroll = DiffScrollState::new(ScrollMode::Margin);
            scroll.set_line_count(cache.lines.len());
            state.commit_diff = BrowseCommitDiffState::Ready {
                annotation: Arc::clone(&annotation),
                cache,
                scroll,
            };
        }
        empty.state = AppState::RepoBrowseFile;
        renders.push(format!("--- empty ---\n{}", render_at(&mut empty, 100, 9)));

        let mut failed = app_with_browse(&["main.rs"]);
        if let Some(state) = failed.browse_state.as_mut() {
            open_file(state, "main.rs", "fn new() {}\n");
            state.commit_diff = BrowseCommitDiffState::Failed {
                annotation,
                message: "git show failed: bad object aaaaaaa".to_string(),
            };
        }
        failed.state = AppState::RepoBrowseFile;
        renders.push(format!(
            "--- failed ---\n{}",
            render_at(&mut failed, 100, 10)
        ));

        assert_snapshot!(renders.join("\n"), @"
        --- loading ---
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files────────────────────────────┐┏▶ aaaaaaa baseline — Alice━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   main.rs                      │┃  ⠋ Loading commit diff…                                       ┃
        │                                 │┃                                                               ┃
        │                                 │┃                                                               ┃
        │                                 │┃                                                               ┃
        └─────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         ⠋ Loading commit diff… | q/Esc back
        --- ready ---
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files────────────────────────────┐┏▶ aaaaaaa baseline — Alice━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   main.rs                      │┃diff --git a/main.rs b/main.rs                                 █
        │                                 │┃index 1111111..2222222 100644                                  █
        │                                 │┃--- a/main.rs                                                  █
        │                                 │┃+++ b/main.rs                                                  █
        │                                 │┃@@ -1 +1 @@                                                    █
        │                                 │┃-fn old() {}                                                   █
        └─────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         j/Down/k/Up scroll | Ctrl-d/Ctrl-u page | gg/G first/last | q/Esc back
        --- empty ---
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files────────────────────────────┐┏▶ aaaaaaa baseline — Alice━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   main.rs                      │┃  This commit has no diff.                                     ┃
        │                                 │┃                                                               ┃
        │                                 │┃                                                               ┃
        └─────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         j/Down/k/Up scroll | Ctrl-d/Ctrl-u page | gg/G first/last | q/Esc back
        --- failed ---
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files────────────────────────────┐┏▶ aaaaaaa baseline — Alice━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   main.rs                      │┃  Commit diff unavailable: git show failed: bad object aaaaaaa ┃
        │                                 │┃                                                               ┃
        │                                 │┃                                                               ┃
        │                                 │┃                                                               ┃
        └─────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         Commit diff unavailable: git show failed: bad object aaaaaaa | q/Esc back
        ");
    }

    #[test]
    fn discussion_failure_respects_existing_footer_priority_and_falls_back_to_status() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        let annotation = blame_annotation(PORCELAIN);
        let failed_discussion = || LineDiscussionState::Failed {
            failure: LineDiscussionFailure::Api,
        };
        let footer = |app: &mut App| {
            render_at(app, 100, 10)
                .lines()
                .last()
                .unwrap_or_default()
                .trim()
                .to_string()
        };

        let mut commit_diff = app_with_browse(&["main.rs"]);
        if let Some(state) = commit_diff.browse_state.as_mut() {
            open_file(state, "main.rs", "one\n");
            state.status = Some("Review comment API failed: rate limited".to_string());
            state.line_discussion = failed_discussion();
            state.commit_diff = BrowseCommitDiffState::Loading {
                request_id: 1,
                annotation: Arc::clone(&annotation),
                cancel: state.cancel_token.child_token(),
            };
        }
        commit_diff.state = AppState::RepoBrowseFile;

        let mut pr_lookup = app_with_browse(&["main.rs"]);
        if let Some(state) = pr_lookup.browse_state.as_mut() {
            open_file(state, "main.rs", "one\n");
            state.status = Some("Review comment API failed: rate limited".to_string());
            state.line_discussion = failed_discussion();
            state.pr_lookup = PrLookupState::Loading {
                request_id: 2,
                sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                cancel: state.cancel_token.child_token(),
            };
        }
        pr_lookup.state = AppState::RepoBrowseFile;

        let mut status = app_with_browse(&["main.rs"]);
        if let Some(state) = status.browse_state.as_mut() {
            open_file(state, "main.rs", "one\n");
            state.status = Some("Review comment API failed: rate limited".to_string());
            state.line_discussion = failed_discussion();
        }
        status.state = AppState::RepoBrowseFile;

        assert_snapshot!(
            format!(
                "commit diff: {}\nPR lookup: {}\nstatus: {}",
                footer(&mut commit_diff),
                footer(&mut pr_lookup),
                footer(&mut status),
            ),
            @"
        commit diff: ⠋ Loading commit diff… | q/Esc back
        PR lookup: ⠋ Looking up pull requests for aaaaaaa…
        status: Review comment API failed: rate limited
        "
        );
    }

    #[test]
    fn test_large_commit_diff_renders_only_the_deep_visible_window() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary large change\n\
             \tone\n";
        let annotation = blame_annotation(PORCELAIN);
        let mut diff = String::from(
            "diff --git a/large.txt b/large.txt\n--- a/large.txt\n+++ b/large.txt\n@@ -0,0 +1,5000 @@\n",
        );
        for line in 1..=5_000 {
            diff.push_str(&format!("+line {line}\n"));
        }

        let mut app = app_with_browse(&["large.txt"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "large.txt", "source\n");
            let cache = crate::ui::diff_view::build_plain_diff_cache(&diff, 4);
            let mut scroll = DiffScrollState::new(ScrollMode::Margin);
            scroll.set_line_count(cache.lines.len());
            scroll.selected_line = cache.lines.len().saturating_sub(2);
            scroll.scroll_offset = cache.lines.len().saturating_sub(5);
            state.commit_diff = BrowseCommitDiffState::Ready {
                annotation,
                cache,
                scroll,
            };
        }
        app.state = AppState::RepoBrowseFile;

        assert_snapshot!(render_at(&mut app, 80, 10), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┏▶ aaaaaaa large change — Alice━━━━━━━━━━━━━━━━━━━━┓
        │▏   large.txt             │┃+line 4996                                        ║
        │                          │┃+line 4997                                        ║
        │                          │┃+line 4998                                        ║
        │                          │┃+line 4999                                        █
        └──────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         j/Down/k/Up scroll | Ctrl-d/Ctrl-u page | gg/G first/last | q/Esc back
        ");
    }

    #[test]
    fn test_commit_diff_footer_uses_configured_first_and_last_bindings() {
        const PORCELAIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time 1700000000\n\
             summary baseline\n\
             \tone\n";
        let mut app = app_with_browse(&["main.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "main.rs", "fn main() {}\n");
            let cache = crate::ui::diff_view::build_plain_diff_cache("+changed\n", 4);
            let mut scroll = DiffScrollState::new(ScrollMode::Margin);
            scroll.set_line_count(cache.lines.len());
            state.commit_diff = BrowseCommitDiffState::Ready {
                annotation: blame_annotation(PORCELAIN),
                cache,
                scroll,
            };
        }
        app.state = AppState::RepoBrowseFile;
        app.config.keybindings.jump_to_first =
            KeySequence::double(KeyBinding::char('z'), KeyBinding::char('z'));
        app.config.keybindings.jump_to_last = KeySequence::single(KeyBinding::char('X'));

        let footer = render_at(&mut app, 100, 8)
            .lines()
            .last()
            .unwrap()
            .trim_end()
            .to_string();
        assert_snapshot!(
            footer,
            @" j/Down/k/Up scroll | Ctrl-d/Ctrl-u page | zz/X first/last | q/Esc back"
        );
    }

    #[test]
    fn test_file_pane_footer_includes_blame_commit_and_back_within_eighty_columns() {
        let mut app = app_with_browse(&["src/app.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "src/app.rs", "fn main() {}\n");
        }
        app.state = AppState::RepoBrowseFile;

        let footer_at = |app: &mut App, width| {
            render_at(app, width, 8)
                .lines()
                .last()
                .unwrap()
                .trim_end()
                .to_string()
        };
        assert_snapshot!(
            format!(
                "80: {}\n60: {}",
                footer_at(&mut app, 80),
                footer_at(&mut app, 60)
            ),
            @"
        80:  o outline | s search | i graph | gb blame | gc diff | gp PR | gr discuss | gd d
        60:  o outline | s search | i graph | gb blame | gc diff | gp PR
        "
        );
    }

    #[test]
    fn test_line_numbers_are_not_shifted_by_a_literal_hunk_header_in_the_file() {
        let mut app = app_with_browse(&["src/header.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "src/header.rs", "first\n@@ -1,1 +1,1 @@\nthird\n");
        }
        app.state = AppState::RepoBrowseFile;

        assert_snapshot!(render_at(&mut app, 80, 12), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┏▶ src/header.rs (1/3)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏ ▼ src/                  │┃    1 first                                       ┃
        │      header.rs           │┃    2 @@ -1,1 +1,1 @@                             ┃
        │                          │┃    3 third                                       ┃
        │                          │┃                                                  ┃
        │                          │┃                                                  ┃
        │                          │┃                                                  ┃
        └──────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         o outline | s search | i graph | gb blame | gc diff | gp PR | gr discuss | gd d
        ");
    }

    #[test]
    fn test_blame_gutter_degrades_full_then_no_time_then_identity_then_hidden() {
        let timestamp = (chrono::Utc::now() - chrono::Duration::hours(2)).timestamp();
        let porcelain = format!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice Example\n\
             author-time {timestamp}\n\
             summary baseline\n\
             \tfn main() {{}}\n"
        );
        let mut renders = Vec::new();
        for (label, width) in [
            ("full", 120),
            ("no time", 90),
            ("identity", 70),
            ("hidden", 50),
        ] {
            let mut app = app_with_browse(&["main.rs"]);
            if let Some(state) = app.browse_state.as_mut() {
                open_file(state, "main.rs", "fn main() {}\n");
                attach_blame(state, "main.rs", &porcelain);
            }
            app.state = AppState::RepoBrowseFile;
            renders.push(format!(
                "--- {label} ({width}) ---\n{}",
                render_at(&mut app, width, 8)
            ));
        }

        assert_snapshot!(renders.join("\n"), @"
        --- full (120) ---
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                                               │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files───────────────────────────────────┐┏▶ main.rs (1/1)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   main.rs                             │┃aaaaaaa Alice Example 2h ago        1 fn main() {}                          ┃
        │                                        │┃                                                                            ┃
        └────────────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         o outline | s search | i graph | gb blame | gc diff | gp PR | gr discuss | gd def | gf edit | q/Esc back
        --- no time (90) ---
        ┌────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                 │
        └────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────────┐┏▶ main.rs (1/1)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   main.rs                   │┃aaaaaaa Alice Example      1 fn main() {}               ┃
        │                              │┃                                                        ┃
        └──────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         o outline | s search | i graph | gb blame | gc diff | gp PR | gr discuss | gd def | gf ed
        --- identity (70) ---
        ┌────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                             │
        └────────────────────────────────────────────────────────────────────┘
        ┌Files──────────────────┐┏▶ main.rs (1/1)━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   main.rs            │┃aaaaaaa         1 fn main() {}             ┃
        │                       │┃                                           ┃
        └───────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         o outline | s search | i graph | gb blame | gc diff | gp PR | gr disc
        --- hidden (50) ---
        ┌────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -         │
        └────────────────────────────────────────────────┘
        ┌Files───────────┐┏▶ main.rs (1/1)━━━━━━━━━━━━━━━┓
        │▏   main.rs     │┃    1 fn main() {}            ┃
        │                │┃                              ┃
        └────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         o outline | s search | i graph | gb blame | gc di
        ");
    }

    #[test]
    fn test_discussion_marker_survives_after_the_blame_gutter_is_hidden() {
        let cache = cache_of("first\nsecond\n");
        let window = content_window(&cache, 0, 2);
        let blame = BlameGutter::from_file(
            parse_porcelain(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 2\n\
                 author Alice\n\
                 summary baseline\n\
                 filename main.rs\n\
                 \tfirst\n\
                 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 2\n\
                 \tsecond\n",
            ),
            2,
        );
        let index = discussion_index(2, 1);

        let rendered: Vec<String> = content_lines(
            &window,
            0,
            false,
            Some(&blame),
            Some(&index),
            20,
            usize::MAX,
        )
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();

        assert!(!rendered.join("\n").contains("aaaaaaa"));
        assert!(rendered[0].starts_with("      1"), "{rendered:?}");
        assert!(rendered[1].starts_with("●     2"), "{rendered:?}");
    }

    #[test]
    fn test_browser_discussion_overlay_reuses_the_review_thread_renderer() {
        let mut app = app_with_browse(&["main.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "main.rs", "first\nsecond\n");
            state.line_discussion = LineDiscussionState::Ready {
                path: "main.rs".to_string(),
                pr_numbers: vec![42],
                index: discussion_index(2, 1),
                view: DiscussionView::ThreadList {
                    line: 1,
                    selected: 0,
                    scroll: 0,
                },
            };
        }
        app.state = AppState::RepoBrowseFile;

        let rendered = render_at(&mut app, 100, 20);
        assert!(rendered.contains("@reviewer on main.rs:2"), "{rendered}");
        assert!(rendered.contains("Discuss this line"), "{rendered}");
    }

    #[test]
    fn open_thread_list_overlay_renders_the_incomplete_index_note() {
        let mut app = app_with_browse(&["main.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "main.rs", "first\nsecond\n");
            let mut index = discussion_index(2, 1);
            index.outcome = DiscussionOutcome::BudgetExhausted {
                max_groups: crate::app::browse_discussion::MAX_DISCUSSION_ANCHOR_GROUPS,
                max_lines: crate::app::browse_discussion::MAX_DISCUSSION_ANCHOR_LINES,
            };
            state.line_discussion = LineDiscussionState::Ready {
                path: "main.rs".to_string(),
                pr_numbers: vec![42],
                index,
                view: DiscussionView::ThreadList {
                    line: 1,
                    selected: 0,
                    scroll: 0,
                },
            };
        }
        app.state = AppState::RepoBrowseFile;

        let rendered = render_at(&mut app, 140, 20);
        assert!(rendered.contains("@reviewer on main.rs:2"), "{rendered}");
        let expected_note =
            "⚠ Incomplete: the anchoring budget was reached (16 groups or 20000 lines maximum)";
        let note = rendered
            .lines()
            .find(|line| line.contains(expected_note))
            .map(|_| expected_note)
            .unwrap_or("<missing>");
        assert_snapshot!(
            note,
            @"⚠ Incomplete: the anchoring budget was reached (16 groups or 20000 lines maximum)"
        );
    }

    #[test]
    fn test_blame_gutter_handles_cjk_repeats_and_uncommitted_lines_honestly() {
        let timestamp = (chrono::Utc::now() - chrono::Duration::hours(2)).timestamp();
        let porcelain = format!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author 山田太郎 Long Name\n\
             author-time {timestamp}\n\
             summary baseline\n\
             \tfirst\n\
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 2 1\n\
             \tsecond\n\
             0000000000000000000000000000000000000000 3 3 1\n\
             author Not Committed Yet\n\
             author-time 0\n\
             summary working tree\n\
             \tthird\n"
        );
        let mut app = app_with_browse(&["main.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "main.rs", "first\nsecond\nthird\n");
            attach_blame(state, "main.rs", &porcelain);
        }
        app.state = AppState::RepoBrowseFile;

        assert_snapshot!(render_at(&mut app, 120, 10), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                                               │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files───────────────────────────────────┐┏▶ main.rs (1/3)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   main.rs                             │┃aaaaaaa 山 田 太 郎  Long N… 2h ago     1 first                                 ┃
        │                                        │┃                                    2 second                                ┃
        │                                        │┃Uncommitted                         3 third                                 ┃
        │                                        │┃                                                                            ┃
        └────────────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         o outline | s search | i graph | gb blame | gc diff | gp PR | gr discuss | gd def | gf edit | q/Esc back
        ");
    }

    #[test]
    fn test_blame_and_buffer_line_count_mismatches_keep_source_alignment() {
        let timestamp = (chrono::Utc::now() - chrono::Duration::days(2)).timestamp();
        let short_porcelain = format!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
             author Alice\n\
             author-time {timestamp}\n\
             summary first\n\
             \tone\n\
             bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 2 2 1\n\
             author Bob\n\
             author-time {timestamp}\n\
             summary second\n\
             \ttwo\n"
        );
        let long_porcelain = format!(
            "{short_porcelain}\
             cccccccccccccccccccccccccccccccccccccccc 3 3 1\n\
             author Carol\n\
             author-time {timestamp}\n\
             summary ignored\n\
             \textra\n"
        );

        let mut short_blame = app_with_browse(&["short.rs"]);
        if let Some(state) = short_blame.browse_state.as_mut() {
            open_file(state, "short.rs", "one\ntwo\nthree\nfour\n");
            attach_blame(state, "short.rs", &short_porcelain);
        }
        short_blame.state = AppState::RepoBrowseFile;

        let mut long_blame = app_with_browse(&["long.rs"]);
        if let Some(state) = long_blame.browse_state.as_mut() {
            open_file(state, "long.rs", "one\ntwo\n");
            attach_blame(state, "long.rs", &long_porcelain);
        }
        long_blame.state = AppState::RepoBrowseFile;

        assert_snapshot!(
            format!(
                "--- blame shorter than buffer ---\n{}\n--- blame longer than buffer ---\n{}",
                render_at(&mut short_blame, 120, 11),
                render_at(&mut long_blame, 120, 9)
            ),
            @"
        --- blame shorter than buffer ---
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                                               │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files───────────────────────────────────┐┏▶ short.rs (1/4)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   short.rs                            │┃aaaaaaa Alice 2d ago                1 one                                   ┃
        │                                        │┃bbbbbbb Bob 2d ago                  2 two                                   ┃
        │                                        │┃[not blamed]                        3 three                                 ┃
        │                                        │┃[not blamed]                        4 four                                  ┃
        │                                        │┃                                                                            ┃
        └────────────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         blame covers 2 lines, this file shows 4 — reopen the file to refresh
        --- blame longer than buffer ---
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                                               │
        └──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files───────────────────────────────────┐┏▶ long.rs (1/2)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   long.rs                             │┃aaaaaaa Alice 2d ago                1 one                                   ┃
        │                                        │┃bbbbbbb Bob 2d ago                  2 two                                   ┃
        │                                        │┃                                                                            ┃
        └────────────────────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         blame covers 3 lines, this file shows 2 — reopen the file to refresh
        "
        );
    }

    #[test]
    fn test_empty_file_with_ready_blame_renders_without_rows_or_panics() {
        let mut app = app_with_browse(&["empty.rs"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(state, "empty.rs", "");
            attach_blame(state, "empty.rs", "");
        }
        app.state = AppState::RepoBrowseFile;

        assert_snapshot!(render_at(&mut app, 80, 8), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┏▶ empty.rs (1/1)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   empty.rs              │┃                                                  ┃
        │                          │┃                                                  ┃
        └──────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         o outline | s search | i graph | gb blame | gc diff | gp PR | gr discuss | gd d
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

        assert_snapshot!(render_at(&mut app, 80, 10), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┏▶ src/long.rs (28/30)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏ ▼ src/                  │┃   27 line 27                                     ║
        │      long.rs             │┃   28 line 28                                     ║
        │                          │┃   29 line 29                                     ║
        │                          │┃   30 line 30                                     █
        └──────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         o outline | s search | i graph | gb blame | gc diff | gp PR | gr discuss | gd d
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

        // The margin clamp centres the cursor (line 101) in the 18-row
        // viewport, so the window runs 93..=110.
        assert!(huge.contains("   93 line 93"), "{huge}");
        assert!(huge.contains("  110 line 110"), "{huge}");
        assert!(!huge.contains("line 111"), "{huge}");
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

        let lines = content_lines(&window, 0, false, None, None, usize::MAX, usize::MAX);
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

        let lines = content_lines(&window, 0, true, None, None, usize::MAX, usize::MAX);

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

        let blame = BlameGutter::from_file(
            parse_porcelain(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 3\n\
                 author Alice\n\
                 author-time 1700000000\n\
                 summary baseline\n\
                 \tfn main() {\n\
                 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2 2\n\
                 \t    println!(\"hi\");\n\
                 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 3 3\n\
                 \t}\n",
            ),
            3,
        );
        let lines = content_lines(&window, 0, true, Some(&blame), None, 120, usize::MAX);
        for line in &lines {
            assert!(
                matches!(line.spans[0].content, Cow::Borrowed(_)),
                "prepared blame text was allocated during rendering"
            );
            assert!(
                matches!(line.spans[1].content, Cow::Owned(_)),
                "the line number remains the only generated span"
            );
            for span in &line.spans[2..] {
                assert!(
                    matches!(span.content, Cow::Borrowed(_)),
                    "source text was copied while rendering blame"
                );
            }
        }
    }

    /// Wrapping must not make a frame O(file): when every visible line wraps
    /// many times, row emission stops at the viewport height instead of
    /// materialising every continuation row of every window line.
    #[test]
    fn test_wrapped_rows_stop_at_the_viewport_row_cap() {
        let long = "x".repeat(1_000);
        let cache = cache_of(&format!("{long}\n{long}\n{long}\n"));
        let window = content_window(&cache, 0, 3);

        let rows = content_lines(&window, 0, false, None, None, 40, 5);

        // content width 40 minus the 6-cell gutter leaves 34 text cells, so
        // each 1,000-cell line alone would produce ~30 rows.
        assert_eq!(rows.len(), 5);
    }

    /// The cursor-line background must extend across wrapped continuation rows
    /// — pad and text alike — otherwise the highlight visually ends at the
    /// first row of a wrapped line.
    #[test]
    fn test_cursor_line_background_covers_wrapped_continuation_rows() {
        let cache = cache_of(&format!("{}\n", "y".repeat(80)));
        let window = content_window(&cache, 0, 4);

        let rows = content_lines(&window, 0, true, None, None, 46, usize::MAX);

        // 46 cells minus the 6-cell gutter wraps 80 glyphs into exactly two rows.
        assert_eq!(rows.len(), 2);
        let text_of = |row: &Line| -> String {
            row.spans[1..]
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert_eq!(text_of(&rows[0]), "y".repeat(40));
        assert_eq!(text_of(&rows[1]), "y".repeat(40));

        let pad = &rows[1].spans[0];
        assert!(
            pad.content.chars().all(|c| c == ' '),
            "the continuation prefix must be blank, not a repeated gutter"
        );
        assert_eq!(pad.content.as_ref().len(), 6);
        assert_eq!(pad.style.bg, Some(CURSOR_LINE_BG));
        assert_eq!(rows[1].spans[1].style.bg, Some(CURSOR_LINE_BG));
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
            content_lines(
                &window,
                total - 1,
                false,
                None,
                None,
                usize::MAX,
                usize::MAX,
            )
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
        assert_snapshot!(render_at(&mut app, 60, 8), @"
        ┌Files──────────────┐┏▶ src/app.rs (1/4)━━━━━━━━━━━━━━━━━━━┓
        │▏ ▼ src/           │┃    1 first                          ┃
        │      app.rs       │┃    2 second                         ┃
        │                   │┃    3 third                          ┃
        │                   │┃    4 fourth                         ┃
        │                   │┃                                     ┃
        │                   │┃                                     ┃
        └───────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
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
    fn test_module_graph_loading_renders_as_a_right_side_pane() {
        let mut app = app_with_browse(&["src/app.ts"]);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/app.ts", "import './helper';\n");
        state.module_graph_pane = ModuleGraphPaneState::Loading {
            request_id: 7,
            path: "src/app.ts".to_string(),
        };
        app.state = AppState::RepoBrowseGraph;

        assert_snapshot!(render_at(&mut app, 100, 12), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files────────────────────────────┐┌src/app.ts (1/1)───────────────┐┏▶ GRAPH (loading…)━━━━━━━━━━━━┓
        │▏ ▼ src/                         ││    1 import './helper';       │┃╔═ current module ═══════════╗┃
        │      app.ts                     ││                               │┃║src/app.ts                  ║┃
        │                                 ││                               │┃╚════════════════════════════╝┃
        │                                 ││                               │┃ Resolving imports and importe┃
        │                                 ││                               │┃                              ┃
        │                                 ││                               │┃                              ┃
        └─────────────────────────────────┘└───────────────────────────────┘┗ Tab/h/l dir | j/k node | Ente┛
         Tab/h/l direction | j/k node | Enter open | i close graph | Esc/q code
        ");
    }

    #[test]
    fn test_module_graph_waiting_keeps_the_right_side_pane_visible() {
        let mut app = app_with_browse(&["src/helper.ts"]);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/helper.ts", "export {};\n");
        state.module_graph_pane = ModuleGraphPaneState::Waiting {
            path: "src/helper.ts".to_string(),
        };
        app.state = AppState::RepoBrowseFile;

        assert_snapshot!(render_at(&mut app, 80, 10), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┏▶ src/helper.ts (1/1)━━━┓┌Graph (opening…)────────┐
        │▏ ▼ src/                  │┃    1 export {};        ┃│╔═ current module ═════╗│
        │      helper.ts           │┃                        ┃│║src/helper.ts         ║│
        │                          │┃                        ┃│╚══════════════════════╝│
        │                          │┃                        ┃│ Waiting for the current│
        └──────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━┛└ i/→ focus ─────────────┘
         o outline | s search | i graph | gb blame | gc diff | gp PR | gr discuss | gd d
        ");
    }

    #[test]
    fn test_tree_focus_does_not_advertise_a_graph_focus_shortcut() {
        let mut app = app_with_browse(&["src/app.ts"]);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/app.ts", "export {};\n");
        state.module_graph_pane = ModuleGraphPaneState::Waiting {
            path: "src/app.ts".to_string(),
        };
        app.state = AppState::RepoBrowseTree;

        let rendered = render_at(&mut app, 100, 10);
        assert!(!rendered.contains("i/→ focus"), "{rendered}");
    }

    #[test]
    fn test_module_graph_pane_hint_uses_the_configured_binding() {
        let mut app = app_with_browse(&["src/app.ts"]);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/app.ts", "export {};\n");
        state.module_graph_pane = ModuleGraphPaneState::Waiting {
            path: "src/app.ts".to_string(),
        };
        app.config.keybindings.module_graph =
            KeySequence::double(KeyBinding::char('m'), KeyBinding::char('i'));
        app.state = AppState::RepoBrowseFile;

        let rendered = render_at(&mut app, 120, 12);
        assert!(rendered.contains("mi/→ focus"), "{rendered}");
    }

    #[test]
    fn test_module_graph_focused_hints_use_configured_controls() {
        let mut app = app_with_browse(&["src/app.ts"]);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/app.ts", "export {};\n");
        state.module_graph_pane = ModuleGraphPaneState::Waiting {
            path: "src/app.ts".to_string(),
        };
        app.config.keybindings.move_left = KeySequence::single(KeyBinding::char('a'));
        app.config.keybindings.move_right = KeySequence::single(KeyBinding::char('d'));
        app.config.keybindings.move_down = KeySequence::single(KeyBinding::char('n'));
        app.config.keybindings.move_up = KeySequence::single(KeyBinding::char('p'));
        app.config.keybindings.open_panel = KeySequence::single(KeyBinding::char(' '));
        app.config.keybindings.module_graph =
            KeySequence::double(KeyBinding::char('m'), KeyBinding::char('i'));
        app.config.keybindings.quit = KeySequence::single(KeyBinding::char('z'));
        app.state = AppState::RepoBrowseGraph;

        let rendered = render_at(&mut app, 160, 12);
        assert!(
            rendered.contains(
                "Tab/a/d direction | n/p node | Space open | mi close graph | Esc/z code"
            ),
            "{rendered}"
        );
    }

    #[test]
    fn test_render_module_graph_outgoing_pane_as_uml_nodes() {
        let mut app = app_with_browse(&["src/app.ts", "src/helper.ts"]);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/app.ts", "import './helper';\n");
        state.module_graph_pane = ModuleGraphPaneState::Ready(ModuleGraphPanel {
            path: "src/app.ts".to_string(),
            display_path: "src/app.ts".to_string(),
            direction: ModuleGraphDirection::Dependencies,
            selected: 0,
            dependencies: ModuleGraphRows {
                rows: vec![
                    ModuleGraphRow {
                        node: "src/helper.ts".to_string(),
                        edge: "[import] ./helper  :1".to_string(),
                        jump: Some(crate::app::browse::ModuleGraphJump {
                            path: "src/helper.ts".to_string(),
                            line: 0,
                        }),
                    },
                    ModuleGraphRow {
                        node: "package react".to_string(),
                        edge: "[import] react  :2".to_string(),
                        jump: None,
                    },
                    ModuleGraphRow {
                        node: "unresolved (not found)".to_string(),
                        edge: "[dynamic] ./missing  :3".to_string(),
                        jump: None,
                    },
                ],
                total: 300,
                guarantee: DependencyGuarantee::Exact,
            },
            dependents: ModuleGraphRows {
                rows: Vec::new(),
                total: 0,
                guarantee: DependencyGuarantee::Exact,
            },
        });
        app.state = AppState::RepoBrowseGraph;

        assert_snapshot!(render_at(&mut app, 100, 18), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  2 files  symbols: -                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files────────────────────────────┐┌src/app.ts (1/1)───────────────┐┏▶ IMPORTS → 3/300 exact━━━━━━━┓
        │▏ ▼ src/                         ││    1 import './helper';       │┃╔═ current module ═══════════╗┃
        │      app.ts                     ││                               │┃║src/app.ts                  ║┃
        │      helper.ts                  ││                               │┃╚════════════════════════════╝┃
        │                                 ││                               │┃│  ┌─ src/helper.ts ─────────┐┃
        │                                 ││                               │┃├─▶│[import] ./helper  :1    │┃
        │                                 ││                               │┃│  └─────────────────────────┘┃
        │                                 ││                               │┃│  ┌─ package react ─────────┐┃
        │                                 ││                               │┃├─▶│[import] react  :2       │┃
        │                                 ││                               │┃│  └─────────────────────────┘┃
        │                                 ││                               │┃│  ┌─ unresolved (not found) ┐┃
        │                                 ││                               │┃└─▶│[dynamic] ./missing  :3  │┃
        │                                 ││                               │┃   └─────────────────────────┘┃
        └─────────────────────────────────┘└───────────────────────────────┘┗ Tab/h/l dir | j/k node | Ente┛
         Tab/h/l direction | j/k node | Enter open | i close graph | Esc/q code
        ");
    }

    #[test]
    fn test_five_column_module_graph_box_stays_within_its_width() {
        let lines = module_graph_box_lines("node", "edge", 5, false);

        assert_eq!(lines[0], "┌───┐");
        assert!(lines
            .iter()
            .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) == 5));
    }

    #[test]
    fn test_render_module_graph_bounded_long_cjk_node() {
        let mut app = app_with_browse(&["src/app.ts"]);
        let node = crate::app::browse::bounded_module_graph_text(&"依存先".repeat(1_000));
        assert!(unicode_width::UnicodeWidthStr::width(node.as_str()) <= 240);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/app.ts", "import './依存先';\n");
        state.module_graph_pane = ModuleGraphPaneState::Ready(ModuleGraphPanel {
            path: "src/app.ts".to_string(),
            display_path: "src/app.ts".to_string(),
            direction: ModuleGraphDirection::Dependencies,
            selected: 0,
            dependencies: ModuleGraphRows {
                rows: vec![ModuleGraphRow {
                    node,
                    edge: "[import] ./依存先  :1".to_string(),
                    jump: None,
                }],
                total: 1,
                guarantee: DependencyGuarantee::Exact,
            },
            dependents: ModuleGraphRows {
                rows: Vec::new(),
                total: 0,
                guarantee: DependencyGuarantee::Exact,
            },
        });
        app.state = AppState::RepoBrowseGraph;

        assert_snapshot!(render_at(&mut app, 80, 12), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┌src/app.ts (1/1)────────┐┏▶ IMPORTS → 1 exact━━━━━┓
        │▏ ▼ src/                  ││    1 import './依 存 先 ';│┃╔═ current module ═════╗┃
        │      app.ts              ││                        │┃║src/app.ts            ║┃
        │                          ││                        │┃╚══════════════════════╝┃
        │                          ││                        │┃│  ┌─ 依 存 先 依 存 先 依 … ─┐┃
        │                          ││                        │┃└─▶│[import] ./依 存 先  …│┃
        │                          ││                        │┃   └───────────────────┘┃
        └──────────────────────────┘└────────────────────────┘┗ Tab/h/l dir | j/k node ┛
         Tab/h/l direction | j/k node | Enter open | i close graph | Esc/q code
        ");
    }

    fn graph_rows(guarantee: DependencyGuarantee) -> ModuleGraphRows {
        ModuleGraphRows {
            rows: (0..3)
                .map(|index| ModuleGraphRow {
                    node: format!("n{index}"),
                    edge: format!("e{index}"),
                    jump: None,
                })
                .collect(),
            total: 300,
            guarantee,
        }
    }

    fn graph_panel(
        direction: ModuleGraphDirection,
        guarantee: DependencyGuarantee,
    ) -> ModuleGraphPanel {
        let filled = graph_rows(guarantee);
        let empty = ModuleGraphRows {
            rows: Vec::new(),
            total: 0,
            guarantee,
        };
        let (dependencies, dependents) = match direction {
            ModuleGraphDirection::Dependencies => (filled, empty),
            ModuleGraphDirection::Dependents => (empty, filled),
        };
        ModuleGraphPanel {
            path: "src/app.ts".to_string(),
            display_path: "src/app.ts".to_string(),
            direction,
            selected: 0,
            dependencies,
            dependents,
        }
    }

    fn render_graph_title(panel: &ModuleGraphPanel, width: u16) -> String {
        let backend = TestBackend::new(width, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::test_default();
        terminal
            .draw(|frame| {
                render_module_graph_ready_pane(
                    frame,
                    ratatui::layout::Rect::new(0, 0, width, 8),
                    panel,
                    Attention::Focused,
                    theme,
                    "help",
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..width).map(|x| buf[(x, 0)].symbol()).collect::<String>()
    }

    #[test]
    fn module_graph_title_keeps_the_guarantee_word_when_the_arrow_fits() {
        let exact = render_graph_title(
            &graph_panel(
                ModuleGraphDirection::Dependencies,
                DependencyGuarantee::Exact,
            ),
            40,
        );
        assert!(exact.contains("▶ IMPORTS → 3/300 exact"), "{exact}");

        let approximate = render_graph_title(
            &graph_panel(
                ModuleGraphDirection::Dependents,
                DependencyGuarantee::Approximate,
            ),
            40,
        );
        assert!(
            approximate.contains("▶ IMPORTERS 3/300 approximate"),
            "{approximate}"
        );
        assert!(!approximate.contains('~'), "{approximate}");
    }

    #[test]
    fn module_graph_title_shrinks_when_the_focused_title_does_not_fit() {
        let exact_imports = render_graph_title(
            &graph_panel(
                ModuleGraphDirection::Dependencies,
                DependencyGuarantee::Exact,
            ),
            22,
        );
        assert!(
            exact_imports.contains("▶ IMPORTS → 3/300"),
            "{exact_imports}"
        );
        assert!(!exact_imports.contains("exact"), "{exact_imports}");
        assert!(!exact_imports.contains('~'), "{exact_imports}");

        let exact_importers = render_graph_title(
            &graph_panel(ModuleGraphDirection::Dependents, DependencyGuarantee::Exact),
            22,
        );
        assert!(
            exact_importers.contains("▶ IMPORTERS 3/300"),
            "{exact_importers}"
        );
        assert!(!exact_importers.contains("exact"), "{exact_importers}");
        assert!(!exact_importers.contains('~'), "{exact_importers}");

        let approximate_imports = render_graph_title(
            &graph_panel(
                ModuleGraphDirection::Dependencies,
                DependencyGuarantee::Approximate,
            ),
            22,
        );
        assert!(
            approximate_imports.contains("▶ IMPORTS → ~3/300"),
            "{approximate_imports}"
        );
        assert!(
            !approximate_imports.contains("approximate"),
            "{approximate_imports}"
        );

        let approximate_importers = render_graph_title(
            &graph_panel(
                ModuleGraphDirection::Dependents,
                DependencyGuarantee::Approximate,
            ),
            22,
        );
        assert!(
            approximate_importers.contains("▶ IMPORTERS ~3/300"),
            "{approximate_importers}"
        );
        assert!(
            !approximate_importers.contains("approximate"),
            "{approximate_importers}"
        );
    }

    #[test]
    fn test_render_module_graph_exact_title_at_80_columns() {
        let mut app = app_with_browse(&["src/app.ts", "src/helper.ts"]);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/app.ts", "import './helper';\n");
        state.module_graph_pane = ModuleGraphPaneState::Ready(ModuleGraphPanel {
            path: "src/app.ts".to_string(),
            display_path: "src/app.ts".to_string(),
            direction: ModuleGraphDirection::Dependencies,
            selected: 0,
            dependencies: ModuleGraphRows {
                rows: vec![
                    ModuleGraphRow {
                        node: "src/helper.ts".to_string(),
                        edge: "[import] ./helper  :1".to_string(),
                        jump: Some(crate::app::browse::ModuleGraphJump {
                            path: "src/helper.ts".to_string(),
                            line: 0,
                        }),
                    },
                    ModuleGraphRow {
                        node: "package react".to_string(),
                        edge: "[import] react  :2".to_string(),
                        jump: None,
                    },
                    ModuleGraphRow {
                        node: "unresolved (not found)".to_string(),
                        edge: "[dynamic] ./missing  :3".to_string(),
                        jump: None,
                    },
                ],
                total: 300,
                guarantee: DependencyGuarantee::Exact,
            },
            dependents: ModuleGraphRows {
                rows: Vec::new(),
                total: 0,
                guarantee: DependencyGuarantee::Exact,
            },
        });
        app.state = AppState::RepoBrowseGraph;

        assert_snapshot!(render_at(&mut app, 80, 18), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  2 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┌src/app.ts (1/1)────────┐┏▶ IMPORTS → 3/300 exact━┓
        │▏ ▼ src/                  ││    1 import './helper';│┃╔═ current module ═════╗┃
        │      app.ts              ││                        │┃║src/app.ts            ║┃
        │      helper.ts           ││                        │┃╚══════════════════════╝┃
        │                          ││                        │┃│  ┌─ src/helper.ts ───┐┃
        │                          ││                        │┃├─▶│[import] ./helper …│┃
        │                          ││                        │┃│  └───────────────────┘┃
        │                          ││                        │┃│  ┌─ package react ───┐┃
        │                          ││                        │┃├─▶│[import] react  :2 │┃
        │                          ││                        │┃│  └───────────────────┘┃
        │                          ││                        │┃│  ┌─ unresolved (not… ┐┃
        │                          ││                        │┃└─▶│[dynamic] ./missin…│┃
        │                          ││                        │┃   └───────────────────┘┃
        └──────────────────────────┘└────────────────────────┘┗ Tab/h/l dir | j/k node ┛
         Tab/h/l direction | j/k node | Enter open | i close graph | Esc/q code
        ");
    }

    #[test]
    fn test_render_module_graph_approximate_title_at_80_columns() {
        let mut app = app_with_browse(&["src/app.ts", "src/helper.ts"]);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/helper.ts", "export const helper = 1;\n");
        state.module_graph_pane = ModuleGraphPaneState::Ready(ModuleGraphPanel {
            path: "src/helper.ts".to_string(),
            display_path: "src/helper.ts".to_string(),
            direction: ModuleGraphDirection::Dependents,
            selected: 0,
            dependencies: ModuleGraphRows {
                rows: Vec::new(),
                total: 0,
                guarantee: DependencyGuarantee::Approximate,
            },
            dependents: ModuleGraphRows {
                rows: vec![ModuleGraphRow {
                    node: "src/app.rs".to_string(),
                    edge: "[use] crate::helper  :4".to_string(),
                    jump: None,
                }],
                total: 1,
                guarantee: DependencyGuarantee::Approximate,
            },
        });
        app.state = AppState::RepoBrowseGraph;

        assert_snapshot!(render_at(&mut app, 80, 14), @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  2 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┌src/helper.ts (1/1)─────┐┏▶ IMPORTERS ~1━━━━━━━━━━┓
        │▏ ▼ src/                  ││    1 export const helpe│┃╔═ current module ═════╗┃
        │      app.ts              ││      r = 1;            │┃║src/helper.ts         ║┃
        │      helper.ts           ││                        │┃╚══════════════════════╝┃
        │                          ││                        │┃│  ┌─ src/app.rs ──────┐┃
        │                          ││                        │┃▲─┘│[use] crate::helpe…│┃
        │                          ││                        │┃   └───────────────────┘┃
        │                          ││                        │┃                        ┃
        │                          ││                        │┃                        ┃
        └──────────────────────────┘└────────────────────────┘┗ Tab/h/l dir | j/k node ┛
         Tab/h/l direction | j/k node | Enter open | i close graph | Esc/q code
        ");
    }

    #[test]
    fn test_render_module_graph_incoming_approximate_pane() {
        let mut app = app_with_browse(&["src/app.ts", "src/helper.ts"]);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/helper.ts", "export const helper = 1;\n");
        state.module_graph_pane = ModuleGraphPaneState::Ready(ModuleGraphPanel {
            path: "src/helper.ts".to_string(),
            display_path: "src/helper.ts".to_string(),
            direction: ModuleGraphDirection::Dependents,
            selected: 0,
            dependencies: ModuleGraphRows {
                rows: Vec::new(),
                total: 0,
                guarantee: DependencyGuarantee::Approximate,
            },
            dependents: ModuleGraphRows {
                rows: vec![ModuleGraphRow {
                    node: "src/app.rs".to_string(),
                    edge: "[use] crate::helper  :4".to_string(),
                    jump: None,
                }],
                total: 1,
                guarantee: DependencyGuarantee::Approximate,
            },
        });
        app.state = AppState::RepoBrowseGraph;

        assert_snapshot!(render_at(&mut app, 100, 14), @"
        ┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  2 files  symbols: -                                                           │
        └──────────────────────────────────────────────────────────────────────────────────────────────────┘
        ┌Files────────────────────────────┐┌src/helper.ts (1/1)────────────┐┏▶ IMPORTERS 1 approximate━━━━━┓
        │▏ ▼ src/                         ││    1 export const helper = 1; │┃╔═ current module ═══════════╗┃
        │      app.ts                     ││                               │┃║src/helper.ts               ║┃
        │      helper.ts                  ││                               │┃╚════════════════════════════╝┃
        │                                 ││                               │┃│  ┌─ src/app.rs ────────────┐┃
        │                                 ││                               │┃▲─┘│[use] crate::helper  :4  │┃
        │                                 ││                               │┃   └─────────────────────────┘┃
        │                                 ││                               │┃                              ┃
        │                                 ││                               │┃                              ┃
        └─────────────────────────────────┘└───────────────────────────────┘┗ Tab/h/l dir | j/k node | Ente┛
         Tab/h/l direction | j/k node | Enter open | i close graph | Esc/q code
        ");
    }

    #[test]
    fn test_empty_module_graph_pane_clips_in_a_tiny_terminal() {
        let mut app = app_with_browse(&["src/empty.ts"]);
        let state = app.browse_state.as_mut().unwrap();
        open_file(state, "src/empty.ts", "export {};\n");
        state.module_graph_pane = ModuleGraphPaneState::Ready(ModuleGraphPanel {
            path: "src/empty.ts".to_string(),
            display_path: "src/empty.ts".to_string(),
            direction: ModuleGraphDirection::Dependencies,
            selected: 0,
            dependencies: ModuleGraphRows {
                rows: Vec::new(),
                total: 0,
                guarantee: DependencyGuarantee::Exact,
            },
            dependents: ModuleGraphRows {
                rows: Vec::new(),
                total: 0,
                guarantee: DependencyGuarantee::Exact,
            },
        });
        app.state = AppState::RepoBrowseGraph;

        assert_snapshot!(render_at(&mut app, 20, 5), @"
        ┌──────────────────┐
        │Repo Browse - demo│
        └──────────────────┘
        ┌Files┐┌src/e┐┏ Tab┓
         Tab/h/l direction |
        ");
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
            let start = (0..buffer.area.width).find(|x| buffer[(*x, y)].symbol() == "▌")?;
            let row: String = (start..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .take_while(|symbol| {
                    !matches!(
                        *symbol,
                        "│" | "┃" | "┌" | "┐" | "└" | "┘" | "┏" | "┓" | "┗" | "┛"
                    )
                })
                .collect();
            let row = row.trim();
            (!row.is_empty()).then(|| row.to_string())
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

    /// A CJK line wider than the pane wraps, and every visual row must break on
    /// a character boundary: half a double-width glyph would either bleed over
    /// the pane border or leave the border painted inside the glyph's cell.
    /// Both alignments matter — with an odd-width prefix the glyph that no longer
    /// fits straddles the border, which is the case that goes wrong.
    #[test]
    fn test_cjk_line_wider_than_the_pane_wraps_on_a_character_boundary() {
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
                "┃",
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

            // The overflow is not discarded: the glyphs that no longer fit
            // continue on the next visual row.
            let continuation_glyphs = (0..border_x)
                .filter(|&x| buf[(x, content_y + 1)].symbol() == "あ")
                .count();
            assert!(
                continuation_glyphs > 0,
                "the overflowing tail must wrap onto the next row: {source}"
            );
        }
    }

    /// The diff view wraps long lines onto continuation rows instead of cutting
    /// them off at the pane edge; the browse content pane matches so markdown
    /// prose and long code lines stay readable.
    #[test]
    fn test_long_line_wraps_onto_continuation_rows() {
        let mut app = app_with_browse(&["notes.md"]);
        if let Some(state) = app.browse_state.as_mut() {
            open_file(
                state,
                "notes.md",
                &format!("{}tail-sentinel\nshort line\n", "word ".repeat(18)),
            );
        }
        app.state = AppState::RepoBrowseFile;
        let out = render_at(&mut app, 80, 12);
        assert!(
            out.contains("tail-sentinel"),
            "the overflowing tail must land on a wrapped continuation row:\n{out}"
        );
        assert_snapshot!(out, @"
        ┌──────────────────────────────────────────────────────────────────────────────┐
        │Repo Browse - demo  1 files  symbols: -                                       │
        └──────────────────────────────────────────────────────────────────────────────┘
        ┌Files─────────────────────┐┏▶ notes.md (1/2)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
        │▏   notes.md              │┃    1 word word word word word word word word word┃
        │                          │┃       word word word word word word word word wor┃
        │                          │┃      d tail-sentinel                             ┃
        │                          │┃    2 short line                                  ┃
        │                          │┃                                                  ┃
        │                          │┃                                                  ┃
        └──────────────────────────┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
         o outline | s search | i graph | gb blame | gc diff | gp PR | gr discuss | gd d
        ");
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
        let paths: Vec<String> = (0..8).map(|i| format!("{i:0>10}日本語")).collect();
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
    /// `clear_overlay_area` has multiple call sites and the sibling test above
    /// covers only the outline's. The symbol-search overlay is wider, so its left
    /// border lands on a different column and needs its own fixture — swapping
    /// its call for a bare `Clear` is invisible to every other test.
    #[test]
    fn test_symbol_search_left_border_survives_a_wide_glyph_straddling_it() {
        let paths: Vec<String> = (0..8).map(|i| format!("{i:0>2}日本語")).collect();
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

        assert_snapshot!(render_at(&mut app, 12, 6), @"
        ┌──────────┐
        │Repo Brows│
        └──────────┘
        ┌Fi┐┏▶ src/┓
        └──┘┗━━━━━━┛
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
