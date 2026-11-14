//! Ratatui rendering.
//!
//! Every widget is drawn from the ANSI palette in [`crate::theme`]. The layout
//! keeps the keyboard model visible: navigation on the left, the active view in
//! the middle, details on the right, playback and prompts at the bottom.

use std::path::Path;
use std::sync::Arc;

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Gauge, LineGauge, List, ListItem, ListState,
    Paragraph, Row, Table, TableState, Tabs, Wrap,
};
use ratatui::Frame;

use crate::app::{
    file_size_label, folder_track_count, track_path_label, App, Focus, Overlay, PlaylistPane,
    PromptKind, StatusKind, TableHitbox, View,
};
use crate::model::{Analysis, Library, Track};
use crate::paths::{display_folder, format_time};
use crate::theme::Theme;
use crate::util::truncate_chars;

const SIDEBAR_WIDTH: u16 = 24;
const CONTEXT_WIDTH: u16 = 34;
const CONTEXT_MIN_WIDTH: u16 = 120;
const PLAYLIST_MIN_SPLIT: u16 = 78;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.width = area.width;
    app.height = area.height;
    app.table_hitbox = None;
    // Wheel scrolling keeps the selection in view, so this only reacts to
    // programmatic jumps (track changes, search matches, item moves).
    app.ensure_active_visible();

    let scanning = app.scan.is_some();
    let mut rows = vec![
        Constraint::Length(2),
        Constraint::Min(6),
        Constraint::Length(3),
        Constraint::Length(2),
    ];
    if scanning {
        rows.insert(2, Constraint::Length(2));
    }
    let chunks = Layout::vertical(rows).split(area);
    let header_area = chunks[0];
    let body_area = chunks[1];
    let scan_area = if scanning { Some(chunks[2]) } else { None };
    let player_area = chunks[chunks.len() - 2];
    let footer_area = chunks[chunks.len() - 1];

    draw_header(frame, app, header_area);
    draw_body(frame, app, body_area);
    if let Some(area) = scan_area {
        draw_scan(frame, app, area);
    }
    draw_player(frame, app, player_area);
    draw_footer(frame, app, footer_area);
    draw_overlay(frame, app, area);
}

// ---------------------------------------------------------------------------
// Shared building blocks
// ---------------------------------------------------------------------------

fn panel<'a>(theme: &Theme, title: impl Into<Line<'a>>) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.border))
        .title(title.into().style(theme.title()))
}

fn empty_state(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: impl Into<Line<'static>>,
    headline: &str,
    hint: &str,
) {
    let block = panel(theme, title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines: Vec<Line> = Vec::new();
    let pad = (inner.height as usize / 2).saturating_sub(2);
    for _ in 0..pad {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "♪",
        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        headline.to_string(),
        theme.strong(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(hint.to_string(), theme.muted())));
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        inner,
    );
}

fn popup(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let width = (area.width * percent_x / 100)
        .min(area.width.saturating_sub(2))
        .max(1);
    let height = (area.height * percent_y / 100)
        .min(area.height.saturating_sub(2))
        .max(1);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

fn fixed_popup(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

fn list_popup(area: Rect, percent_x: u16, rows: usize, max_percent_y: u16) -> Rect {
    // List popups stay readable on narrow terminals: prefer the requested
    // percentage, but never drop below 64 columns when the screen allows it.
    let preferred = (area.width * percent_x / 100).min(area.width.saturating_sub(2));
    let floor = 64.min(area.width.saturating_sub(2));
    let width = preferred.max(floor).max(1);
    let max_height = (area.height * max_percent_y / 100)
        .min(area.height.saturating_sub(2))
        .max(7);
    let height = (rows as u16).saturating_add(4).min(max_height).max(7);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

fn status_color(kind: StatusKind, theme: &Theme) -> Color {
    match kind {
        StatusKind::Info => theme.accent,
        StatusKind::Working => theme.warn,
        StatusKind::Success => theme.good,
        StatusKind::Error => theme.error,
    }
}

fn badge_style(kind: StatusKind, theme: &Theme) -> Style {
    Style::new()
        .fg(status_color(kind, theme))
        .add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

fn stars(rating: i32) -> (String, String) {
    let filled = rating.clamp(0, 5) as usize;
    ("★".repeat(filled), "☆".repeat(5 - filled))
}

fn number(value: f64) -> String {
    if !value.is_finite() {
        return "—".to_string();
    }
    if value.abs() >= 1000.0 {
        format!("{value:.0}")
    } else if value.abs() >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn json_label(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "—".to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(|value| value.to_string())
            .or_else(|| value.as_u64().map(|value| value.to_string()))
            .or_else(|| value.as_f64().map(number))
            .unwrap_or_else(|| value.to_string()),
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Array(items) => format!("[{} values]", items.len()),
        serde_json::Value::Object(_) => {
            let text = serde_json::to_string(value).unwrap_or_default();
            truncate_chars(&text, 56)
        }
    }
}

fn small_label(theme: &Theme, label: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), theme.muted()),
        Span::styled(value.into(), theme.strong()),
    ])
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::new().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let wide = area.width >= 110;
    let compact = area.width < 72;
    let mut constraints = Vec::new();
    if !compact {
        constraints.push(Constraint::Length(11));
    }
    constraints.push(Constraint::Min(24));
    if wide {
        constraints.push(Constraint::Length(50));
    } else if area.width >= 88 {
        constraints.push(Constraint::Length(20));
    }
    let chunks = Layout::horizontal(constraints).split(inner);

    let mut index = 0;
    if !compact {
        let brand = Paragraph::new(Line::from(vec![
            Span::styled(
                "◆ ",
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled("gbonds", theme.title()),
        ]));
        frame.render_widget(brand, chunks[index]);
        index += 1;
    }

    let titles: Vec<Line> = View::ALL
        .iter()
        .map(|view| {
            Line::from(vec![
                Span::styled(format!("{} ", view.number()), theme.muted()),
                Span::raw(view.label().to_string()),
            ])
        })
        .collect();
    let selected = View::ALL
        .iter()
        .position(|view| *view == app.view)
        .unwrap_or(0);
    let tabs = Tabs::new(titles)
        .select(Some(selected))
        .style(theme.muted())
        .highlight_style(Style::new().fg(theme.accent).add_modifier(Modifier::BOLD))
        .divider(Span::styled("  ", theme.muted()))
        .padding("", " ");
    frame.render_widget(tabs, chunks[index]);
    index += 1;

    if index < chunks.len() {
        let stats = if wide {
            format!(
                "{} tracks · {} links · watch {} · {} ",
                app.total_tracks(),
                app.edge_count(),
                if app.folders.watch_enabled {
                    "on"
                } else {
                    "off"
                },
                app.theme_mode.label()
            )
        } else {
            format!("{} tracks ", app.total_tracks())
        };
        let paragraph = Paragraph::new(Line::from(Span::styled(stats, theme.muted())))
            .alignment(Alignment::Right);
        frame.render_widget(paragraph, chunks[index]);
    }
}

// ---------------------------------------------------------------------------
// Body: sidebar, active view, details
// ---------------------------------------------------------------------------

fn draw_body(frame: &mut Frame, app: &mut App, area: Rect) {
    let show_sidebar = app.sidebar_visible();
    let show_context = area.width >= CONTEXT_MIN_WIDTH;
    let mut constraints = Vec::new();
    if show_sidebar {
        constraints.push(Constraint::Length(SIDEBAR_WIDTH));
    }
    constraints.push(Constraint::Min(30));
    if show_context {
        constraints.push(Constraint::Length(CONTEXT_WIDTH));
    }
    let chunks = Layout::horizontal(constraints).split(area);
    let mut index = 0;
    if show_sidebar {
        draw_sidebar(frame, app, chunks[index]);
        index += 1;
    }
    let main_area = chunks[index];
    index += 1;
    let context_area = if show_context {
        Some(chunks[index])
    } else {
        None
    };

    // Read through a cloned handle so the UI can mutate App state while the
    // library guard is alive.
    let handle = Arc::clone(&app.library);
    let library = match handle.read() {
        Ok(library) => library,
        Err(poisoned) => poisoned.into_inner(),
    };

    draw_view(frame, app, &library, main_area);
    if let Some(area) = context_area {
        draw_context(frame, app, &library, area);
    }
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    let library = match app.library.read() {
        Ok(library) => library,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_row = None;
    let mut entry_index = 0usize;

    let sidebar_section = |title: &str| {
        ListItem::new(Line::from(Span::styled(
            title.to_string(),
            Style::new().fg(theme.dim).add_modifier(Modifier::BOLD),
        )))
    };

    items.push(sidebar_section("VIEWS"));
    for view in View::ALL {
        if entry_index == app.sidebar.index {
            selected_row = Some(items.len());
        }
        let active = app.view == view;
        let marker = if active { "●" } else { " " };
        let style = if active {
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.text)
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{marker} "), style),
            Span::styled(format!("{} {}", view.number(), view.label()), style),
        ])));
        entry_index += 1;
    }

    let names = app.playlists.names();
    if !names.is_empty() {
        items.push(ListItem::new(""));
        items.push(sidebar_section("PLAYLISTS"));
        for name in &names {
            if entry_index == app.sidebar.index {
                selected_row = Some(items.len());
            }
            let count = app.playlists.count(name);
            items.push(ListItem::new(Line::from(vec![
                Span::styled("  ", theme.muted()),
                Span::raw(name.clone()),
                Span::styled(format!("  {count}"), theme.muted()),
            ])));
            entry_index += 1;
        }
    }

    if !app.folders.folders.is_empty() {
        items.push(ListItem::new(""));
        items.push(sidebar_section("FOLDERS"));
        for folder in &app.folders.folders {
            if entry_index == app.sidebar.index {
                selected_row = Some(items.len());
            }
            let count = folder_track_count(&library, folder);
            let label = truncate_chars(&display_folder(folder), 17);
            items.push(ListItem::new(Line::from(vec![
                Span::styled("  ", theme.muted()),
                Span::raw(label),
                Span::styled(format!("  {count}"), theme.muted()),
            ])));
            entry_index += 1;
        }
    }

    let focus_style = if app.focus == Focus::Sidebar {
        theme.selection
    } else {
        theme.selection_inactive
    };
    let block = panel(&theme, " Browse ")
        .title_bottom(Line::from(Span::styled(
            " o folder · O manage ",
            theme.muted(),
        )))
        .padding(ratatui::widgets::Padding::new(1, 1, 0, 0));
    let list = List::new(items).block(block).highlight_style(focus_style);
    let mut state = ListState::default().with_selected(selected_row);
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_view(frame: &mut Frame, app: &mut App, library: &Library, area: Rect) {
    match app.view {
        View::Library => draw_library(frame, app, library, area),
        View::ForYou => draw_for_you(frame, app, library, area),
        View::Queue => draw_queue(frame, app, library, area),
        View::Playlists => draw_playlists(frame, app, library, area),
    }
}

fn draw_library(frame: &mut Frame, app: &mut App, library: &Library, area: Rect) {
    let theme = app.theme;
    let total = app.library_state.ids.len();
    let query = app.library_state.query.clone();
    let mut title = format!(" Library · {total} ");
    if !query.is_empty() {
        title = format!(" Library · {total} · /{} ", truncate_chars(&query, 24));
    }
    if total == 0 {
        let (headline, hint) = if query.is_empty() {
            (
                "No tracks yet",
                "Press o to add a music folder, then :scan to import files",
            )
        } else {
            (
                "Nothing matches this filter",
                "Press Esc to clear the search, or / to edit it",
            )
        };
        empty_state(frame, area, &theme, title, headline, hint);
        return;
    }
    let selected = app.library_state.selected;
    let offset = app.library_state.offset;
    let current = app.current_track_id();
    let rendered = render_track_table(
        frame,
        area,
        &theme,
        library,
        &app.library_state.ids,
        selected,
        offset,
        current.as_deref(),
        panel(&theme, title),
        TableKind::Library,
        app.focus == Focus::Content,
    );
    app.library_state.offset = rendered.offset;
    app.library_state.visible_rows = rendered.visible_rows;
    app.table_hitbox = Some(rendered.hitbox);
}

fn draw_for_you(frame: &mut Frame, app: &mut App, library: &Library, area: Rect) {
    let theme = app.theme;
    let Some(seed) = app.recommendations.seed_id.clone() else {
        empty_state(
            frame,
            area,
            &theme,
            " For You ",
            "Recommendations live here",
            "Select an analyzed track and press r, or press R for radio",
        );
        return;
    };
    let label = library.find(&seed).map(Track::title_artist).unwrap_or(seed);
    let title = format!(" For You · {} ", truncate_chars(&label, 36));
    if app.recommendations.ids.is_empty() {
        empty_state(
            frame,
            area,
            &theme,
            title,
            "No similar tracks found",
            "Try a track with audio analysis, or rebuild with :analyze",
        );
        return;
    }
    let selected = app.recommendations.selected;
    let offset = app.recommendations.offset;
    let current = app.current_track_id();
    let rendered = render_track_table(
        frame,
        area,
        &theme,
        library,
        &app.recommendations.ids,
        selected,
        offset,
        current.as_deref(),
        panel(&theme, title),
        TableKind::Recommendation,
        app.focus == Focus::Content,
    );
    app.recommendations.offset = rendered.offset;
    app.recommendations.visible_rows = rendered.visible_rows;
    app.table_hitbox = Some(rendered.hitbox);
}

fn draw_queue(frame: &mut Frame, app: &mut App, library: &Library, area: Rect) {
    let theme = app.theme;
    let queue = &app.player.queue;
    let state_label = format!(
        " · {} · shuffle {} · repeat {} ",
        app.player.state(),
        if app.player.shuffle { "on" } else { "off" },
        app.player.repeat
    );
    let title = format!(" Queue · {} tracks{state_label}", queue.len());
    if queue.is_empty() {
        empty_state(
            frame,
            area,
            &theme,
            title,
            "The queue is empty",
            "Press e on a track, or R for a radio queue",
        );
        return;
    }
    let selected = app.queue_selected;
    let offset = app.queue_offset;
    let current = app.current_track_id();
    let rendered = render_track_table(
        frame,
        area,
        &theme,
        library,
        queue,
        selected,
        offset,
        current.as_deref(),
        panel(&theme, title),
        TableKind::Queue,
        app.focus == Focus::Content,
    );
    app.queue_offset = rendered.offset;
    app.queue_visible_rows = rendered.visible_rows;
    app.table_hitbox = Some(rendered.hitbox);
}

fn draw_playlists(frame: &mut Frame, app: &mut App, library: &Library, area: Rect) {
    let theme = app.theme;
    let names = app.playlists.names();
    if names.is_empty() {
        empty_state(
            frame,
            area,
            &theme,
            " Playlists ",
            "No playlists yet",
            "Press a to create one, or A to add the whole current list",
        );
        return;
    }

    if area.width >= PLAYLIST_MIN_SPLIT {
        let chunks = Layout::horizontal([Constraint::Length(27), Constraint::Min(30)]).split(area);
        draw_playlist_list(frame, app, chunks[0]);
        draw_playlist_tracks(frame, app, library, chunks[1]);
    } else {
        let chunks = Layout::vertical([Constraint::Length(8), Constraint::Min(6)]).split(area);
        draw_playlist_list(frame, app, chunks[0]);
        draw_playlist_tracks(frame, app, library, chunks[1]);
    }
}

fn draw_playlist_list(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    let names = app.playlists.names();
    let items: Vec<ListItem> = names
        .iter()
        .map(|name| {
            ListItem::new(Line::from(vec![
                Span::raw(name.clone()),
                Span::styled(format!("  {}", app.playlists.count(name)), theme.muted()),
            ]))
        })
        .collect();
    let index = app
        .playlists_state
        .playlist_index
        .min(names.len().saturating_sub(1));
    let active = app.focus == Focus::Content && app.playlist_pane == PlaylistPane::List;
    let style = if active {
        theme.selection
    } else {
        theme.selection_inactive
    };
    let block = panel(&theme, format!(" Playlists · {} ", names.len())).title_bottom(Line::from(
        Span::styled(" a new · r rename · d delete ", theme.muted()),
    ));
    let list = List::new(items).block(block).highlight_style(style);
    let mut state = ListState::default().with_selected(Some(index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_playlist_tracks(frame: &mut Frame, app: &mut App, library: &Library, area: Rect) {
    let theme = app.theme;
    let name = app
        .current_playlist_name()
        .unwrap_or_else(|| "Playlist".to_string());
    let ids = &app.playlists_state.ids;
    let title = format!(" {} · {} tracks ", truncate_chars(&name, 34), ids.len());
    if ids.is_empty() {
        empty_state(
            frame,
            area,
            &theme,
            title,
            "This playlist is empty",
            "Select tracks in the library and press a to add them",
        );
        return;
    }
    let active = app.focus == Focus::Content && app.playlist_pane == PlaylistPane::Tracks;
    let selected = app.playlists_state.track_index;
    let offset = app.playlists_state.track_offset;
    let current = app.current_track_id();
    let rendered = render_track_table(
        frame,
        area,
        &theme,
        library,
        ids,
        selected,
        offset,
        current.as_deref(),
        panel(&theme, title),
        TableKind::Playlist,
        active,
    );
    app.playlists_state.track_offset = rendered.offset;
    app.playlists_state.track_visible_rows = rendered.visible_rows;
    app.table_hitbox = Some(rendered.hitbox);
}

// ---------------------------------------------------------------------------
// Track tables
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TableKind {
    Library,
    Recommendation,
    Queue,
    Playlist,
}

struct RenderedTable {
    offset: usize,
    visible_rows: usize,
    hitbox: TableHitbox,
}

fn render_track_table(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    library: &Library,
    ids: &[String],
    selected: usize,
    offset: usize,
    current: Option<&str>,
    block: Block<'static>,
    kind: TableKind,
    focused: bool,
) -> RenderedTable {
    let inner = block.inner(area);
    let visible_rows = inner.height.saturating_sub(1) as usize;
    let inner_width = area.width.saturating_sub(2);
    let library_layout = kind == TableKind::Library;
    let wide = inner_width >= 94;
    let medium = inner_width >= 66;

    let mut widths: Vec<Constraint> = Vec::new();
    let mut headers: Vec<&'static str> = Vec::new();
    if library_layout {
        if wide {
            widths.extend([
                Constraint::Length(6),
                Constraint::Percentage(38),
                Constraint::Percentage(22),
                Constraint::Percentage(13),
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Length(6),
            ]);
            headers.extend(["ID", "Title", "Artist", "Genre", "BPM", "★", "Time"]);
        } else if medium {
            widths.extend([
                Constraint::Length(5),
                Constraint::Min(18),
                Constraint::Length(16),
                Constraint::Length(10),
                Constraint::Length(6),
            ]);
            headers.extend(["ID", "Title", "Artist", "Genre", "Time"]);
        } else {
            widths.extend([
                Constraint::Min(16),
                Constraint::Length(12),
                Constraint::Length(6),
            ]);
            headers.extend(["Title", "Artist", "Time"]);
        }
    } else if wide {
        widths.extend([
            Constraint::Length(4),
            Constraint::Percentage(40),
            Constraint::Percentage(24),
            Constraint::Percentage(13),
            Constraint::Length(5),
            Constraint::Length(6),
        ]);
        headers.extend(["#", "Title", "Artist", "Genre", "BPM", "Time"]);
    } else if medium {
        widths.extend([
            Constraint::Length(4),
            Constraint::Min(18),
            Constraint::Length(16),
            Constraint::Length(10),
            Constraint::Length(6),
        ]);
        headers.extend(["#", "Title", "Artist", "Genre", "Time"]);
    } else {
        widths.extend([
            Constraint::Length(4),
            Constraint::Min(16),
            Constraint::Length(12),
            Constraint::Length(6),
        ]);
        headers.extend(["#", "Title", "Artist", "Time"]);
    }

    let mut rows: Vec<Row> = Vec::with_capacity(ids.len());
    for (index, id) in ids.iter().enumerate() {
        let track = library.find(id);
        let is_current = current == Some(id.as_str());
        rows.push(build_row(
            theme, track, id, index, is_current, kind, &headers,
        ));
    }

    let header = Row::new(headers)
        .style(Style::new().fg(theme.dim).add_modifier(Modifier::BOLD))
        .bottom_margin(0);
    let highlight = if focused {
        theme.selection
    } else {
        theme.selection_inactive
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(highlight);
    let selected = if ids.is_empty() {
        0
    } else {
        selected.min(ids.len() - 1)
    };
    // Only pass a selection to ratatui while it is inside the viewport.
    // Otherwise the table would snap the viewport back to the selection and
    // make independent wheel scrolling impossible.
    let offset = offset.min(ids.len().saturating_sub(visible_rows.max(1)));
    let in_view = visible_rows > 0 && selected >= offset && selected < offset + visible_rows;
    let mut state = TableState::default()
        .with_offset(offset)
        .with_selected(in_view.then_some(selected));
    frame.render_stateful_widget(table, area, &mut state);

    let displayed = visible_rows.min(ids.len().saturating_sub(state.offset()));
    let view = match kind {
        TableKind::Library => View::Library,
        TableKind::Recommendation => View::ForYou,
        TableKind::Queue => View::Queue,
        TableKind::Playlist => View::Playlists,
    };
    RenderedTable {
        offset: state.offset(),
        visible_rows,
        hitbox: TableHitbox {
            view,
            pane: PlaylistPane::Tracks,
            x: inner.x,
            y: inner.y.saturating_add(1),
            width: inner.width,
            height: displayed.min(u16::MAX as usize) as u16,
            offset: state.offset(),
            rows: ids.len(),
        },
    }
}

fn build_row(
    theme: &Theme,
    track: Option<&Track>,
    id: &str,
    index: usize,
    is_current: bool,
    kind: TableKind,
    headers: &[&str],
) -> Row<'static> {
    let library_layout = kind == TableKind::Library;
    let wide = if library_layout {
        headers.len() >= 7
    } else {
        headers.len() >= 6
    };
    let medium = headers.len() >= 5;
    let title = track
        .map(|track| track.title.clone())
        .unwrap_or_else(|| id.to_string());
    let artist = track.map(|track| track.artist.clone()).unwrap_or_default();
    let genre = track
        .map(|track| track.display_genre().to_string())
        .unwrap_or_default();
    let duration = track.map(Track::duration_seconds).unwrap_or(0.0);
    let bpm = track.map(Track::bpm).unwrap_or(0.0);

    let title_cell = if library_layout {
        if is_current {
            Cell::from(Line::from(vec![
                Span::styled(
                    "▶ ",
                    Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
                ),
                Span::raw(title),
            ]))
        } else {
            Cell::from(title)
        }
    } else {
        Cell::from(title)
    };

    let mut cells: Vec<Cell> = Vec::new();
    if wide {
        if library_layout {
            cells.push(Cell::from(id.to_string()));
        } else {
            cells.push(Cell::from(rank_cell(theme, index, is_current)));
        }
        cells.push(title_cell);
        cells.push(Cell::from(artist));
        cells.push(Cell::from(if genre.is_empty() {
            "—".to_string()
        } else {
            genre
        }));
        cells.push(Cell::from(if bpm > 0.0 {
            number(bpm)
        } else {
            "—".to_string()
        }));
        if library_layout {
            let (filled, empty) = stars(track.map(|track| track.rating).unwrap_or(0));
            cells.push(Cell::from(Line::from(vec![
                Span::styled(filled, Style::new().fg(theme.warn)),
                Span::styled(empty, Style::new().fg(theme.dim)),
            ])));
        }
        cells.push(Cell::from(format_time(duration)));
    } else if medium {
        if library_layout {
            cells.push(Cell::from(id.to_string()));
        } else {
            cells.push(Cell::from(rank_cell(theme, index, is_current)));
        }
        cells.push(title_cell);
        cells.push(Cell::from(artist));
        cells.push(Cell::from(if genre.is_empty() {
            "—".to_string()
        } else {
            genre
        }));
        cells.push(Cell::from(format_time(duration)));
    } else {
        if !library_layout {
            cells.push(Cell::from(rank_cell(theme, index, is_current)));
        }
        cells.push(title_cell);
        cells.push(Cell::from(artist));
        cells.push(Cell::from(format_time(duration)));
    }
    Row::new(cells)
}

fn rank_cell(theme: &Theme, index: usize, is_current: bool) -> Line<'static> {
    if is_current {
        Line::from(Span::styled(
            "▶",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            format!("{}", index + 1),
            Style::new().fg(theme.dim),
        ))
    }
}

// ---------------------------------------------------------------------------
// Details pane
// ---------------------------------------------------------------------------

fn draw_context(frame: &mut Frame, app: &App, library: &Library, area: Rect) {
    let theme = app.theme;
    let mut lines: Vec<Line> = Vec::new();

    let selected = app.context.selected_id.as_deref();
    let playing = app.current_track_id();
    let target = selected
        .and_then(|id| library.find(id))
        .or_else(|| playing.as_deref().and_then(|id| library.find(id)));

    if let Some(track) = target {
        let heading = if playing.as_deref() == Some(track.id.as_str()) {
            "NOW PLAYING"
        } else {
            "SELECTED"
        };
        lines.push(Line::from(Span::styled(heading, theme.muted())));
        lines.push(Line::from(Span::styled(
            track.title.clone(),
            Style::new().fg(theme.text).add_modifier(Modifier::BOLD),
        )));
        if !track.artist.is_empty() {
            lines.push(Line::from(Span::styled(
                track.artist.clone(),
                Style::new().fg(theme.accent),
            )));
        }
        lines.push(Line::from(""));
        let (filled, empty) = stars(track.rating);
        lines.push(Line::from(vec![
            Span::styled(filled, Style::new().fg(theme.warn)),
            Span::styled(empty, Style::new().fg(theme.dim)),
            Span::styled(
                format!("  {}", track.display_genre()),
                Style::new().fg(theme.dim),
            ),
        ]));
        lines.push(small_label(
            &theme,
            "Duration",
            format_time(track.duration_seconds()),
        ));
        let bpm = track.bpm();
        if bpm > 0.0 {
            lines.push(small_label(&theme, "Tempo", format!("{} BPM", number(bpm))));
        }
        let key = track.key();
        let scale = track.scale();
        if !key.is_empty() {
            lines.push(small_label(
                &theme,
                "Key",
                if scale.is_empty() {
                    key.to_string()
                } else {
                    format!("{key} {scale}")
                },
            ));
        }
        if let Some(album) = track.album.as_deref().filter(|album| !album.is_empty()) {
            let album = match track.year {
                Some(year) => format!("{album} ({year})"),
                None => album.to_string(),
            };
            lines.push(small_label(&theme, "Album", album));
        }
        let mood = track.mood();
        if !mood.is_empty() {
            lines.push(small_label(&theme, "Mood", mood));
        }
        let energy = track.analysis_f64("energy");
        let dance = track.analysis_f64("danceability");
        let valence = track.analysis_f64("valence");
        if energy > 0.0 || dance > 0.0 || valence > 0.0 {
            lines.push(Line::from(""));
            lines.push(meter_line(&theme, "Energy", energy));
            lines.push(meter_line(&theme, "Dance", dance));
            lines.push(meter_line(&theme, "Valence", valence));
        }
        lines.push(Line::from(""));
        let path = display_folder(&track_path_label(track));
        lines.push(Line::from(Span::styled(
            truncate_chars(&path, 120),
            Style::new().fg(theme.dim),
        )));

        if !app.context.recs.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "SIMILAR",
                Style::new().fg(theme.dim).add_modifier(Modifier::BOLD),
            )));
            for (index, id) in app.context.recs.iter().take(6).enumerate() {
                if let Some(track) = library.find(id) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{} ", index + 1), theme.muted()),
                        Span::raw(truncate_chars(
                            &format!("{} — {}", track.artist, track.title),
                            28,
                        )),
                    ]));
                }
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "LIBRARY",
            Style::new().fg(theme.dim).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            format!("{} tracks", library.tracks.len()),
            theme.strong(),
        )));
        lines.push(small_label(
            &theme,
            "Links",
            library.graph.edge_count().to_string(),
        ));
        lines.push(small_label(
            &theme,
            "Folders",
            app.folders.folders.len().to_string(),
        ));
        lines.push(small_label(
            &theme,
            "Playlists",
            app.playlists.names().len().to_string(),
        ));
        lines.push(small_label(
            &theme,
            "Watch",
            if app.folders.watch_enabled {
                "on"
            } else {
                "off"
            },
        ));
        lines.push(small_label(&theme, "Theme", app.theme.resolved));
        lines.push(small_label(&theme, "Backend", app.player.backend_name()));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "FOLDERS",
            Style::new().fg(theme.dim).add_modifier(Modifier::BOLD),
        )));
        if app.folders.folders.is_empty() {
            lines.push(Line::from(Span::styled(
                "No folders connected",
                theme.muted(),
            )));
            lines.push(Line::from(Span::styled(
                "Press o to add music",
                theme.muted(),
            )));
        } else {
            for folder in &app.folders.folders {
                let count = folder_track_count(library, folder);
                lines.push(Line::from(vec![
                    Span::raw(format!("{} ", truncate_chars(&display_folder(folder), 24))),
                    Span::styled(format!("{count}"), theme.muted()),
                ]));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Press ? for the full key map",
            theme.muted(),
        )));
    }

    let block = panel(&theme, " Details ");
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn meter_line(theme: &Theme, label: &str, value: f64) -> Line<'static> {
    let value = value.clamp(0.0, 1.0);
    let width = 10usize;
    let filled = (value * width as f64).round() as usize;
    Line::from(vec![
        Span::styled(format!("{label:<9}"), theme.muted()),
        Span::styled("█".repeat(filled), Style::new().fg(theme.accent)),
        Span::styled("░".repeat(width - filled), Style::new().fg(theme.dim)),
        Span::styled(format!(" {:>3.0}%", value * 100.0), theme.muted()),
    ])
}

// ---------------------------------------------------------------------------
// Scan progress, player, footer
// ---------------------------------------------------------------------------

fn draw_scan(frame: &mut Frame, app: &App, area: Rect) {
    let Some(scan) = app.scan.as_ref() else {
        return;
    };
    let theme = app.theme;
    let progress = if scan.total > 0 {
        scan.processed as f64 / scan.total as f64
    } else if scan.percent > 0.0 {
        scan.percent / 100.0
    } else {
        0.0
    }
    .clamp(0.0, 1.0);
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    let label = format!(
        " {} · {:.0}% · {} / {} {} ",
        scan.stage,
        progress * 100.0,
        scan.processed,
        scan.total,
        scan.message
    );
    let gauge = Gauge::default()
        .ratio(progress)
        .label(Span::styled(label, theme.strong()))
        .gauge_style(Style::new().fg(theme.accent).bg(theme.dim));
    frame.render_widget(gauge, chunks[0]);

    let current = if scan.current_name.is_empty() {
        scan.message.clone()
    } else {
        format!("{} — {}", scan.current_name, display_folder(&scan.current))
    };
    let detail = Line::from(vec![
        Span::styled(
            format!(
                " {:.0}s · {} failed · {} links ",
                scan.elapsed, scan.failed, scan.edges
            ),
            theme.muted(),
        ),
        Span::styled(truncate_chars(&current, 100), theme.muted()),
    ]);
    frame.render_widget(Paragraph::new(detail), chunks[1]);
}

fn draw_player(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(theme.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);

    let state = app.player.state();
    let (icon, icon_style) = match state {
        "playing" => (
            "▶",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        "paused" => ("⏸", Style::new().fg(theme.warn)),
        _ => ("■", Style::new().fg(theme.dim)),
    };
    let label = app
        .player
        .current_track()
        .map(|track| track.title_artist())
        .unwrap_or_else(|| "Nothing playing — select a track and press Enter".to_string());
    let label = truncate_chars(&label, inner.width.saturating_sub(28) as usize);
    let time = format!(
        "{}  ·  vol {}%",
        format_time(app.player.position()) + " / " + &format_time(app.player.duration()),
        app.player.volume
    );

    let label_area = Layout::horizontal([
        Constraint::Min(10),
        Constraint::Length(time.chars().count() as u16 + 1),
    ])
    .split(chunks[0]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {icon} "), icon_style),
            Span::styled(
                label,
                if state == "playing" {
                    Style::new().fg(theme.text).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.dim)
                },
            ),
        ])),
        label_area[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(time, theme.muted()))).alignment(Alignment::Right),
        label_area[1],
    );

    let ratio = if app.player.duration() > 0.0 {
        (app.player.position() / app.player.duration()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let gauge = LineGauge::default()
        .ratio(ratio)
        .label(Line::from(""))
        .filled_style(Style::new().fg(theme.accent))
        .unfilled_style(Style::new().fg(theme.dim))
        .filled_symbol("━")
        .unfilled_symbol("─");
    frame.render_widget(gauge, chunks[1]);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);

    if let Some(prompt) = app.prompt.as_ref() {
        draw_prompt(frame, app, prompt, chunks[0]);
        let hint = prompt_hint(&prompt.kind);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, theme.muted()))),
            chunks[1],
        );
        return;
    }

    let status = if app.status.is_empty() {
        "Ready"
    } else {
        app.status.as_str()
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", app.mode_label()),
            badge_style(app.status_kind, &theme),
        ),
        Span::raw(" "),
        Span::styled(
            status.to_string(),
            StatusLineStyle::style(app.status_kind, &theme),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), chunks[0]);

    let wide = area.width >= 104;
    let hint = if wide {
        " j/k move · Enter play · space pause · r similar · R radio · a playlist · o folder · / search · : cmd · ? help · q quit"
    } else {
        " j/k move · Enter play · space pause · / search · : command · ? help · q quit"
    }
    .to_string();
    let right = format!(
        "{} · v{} ",
        app.theme_mode.label(),
        env!("CARGO_PKG_VERSION")
    );
    if area.width >= 76 {
        let layout =
            Layout::horizontal([Constraint::Min(30), Constraint::Length(16)]).split(chunks[1]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, theme.muted()))),
            layout[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(right, theme.muted())))
                .alignment(Alignment::Right),
            layout[1],
        );
    } else {
        let hint = truncate_chars(&hint, area.width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, theme.muted()))),
            chunks[1],
        );
    }
}

struct StatusLineStyle;

impl StatusLineStyle {
    fn style(kind: StatusKind, theme: &Theme) -> Style {
        match kind {
            StatusKind::Error => Style::new().fg(theme.error),
            StatusKind::Success => Style::new().fg(theme.good),
            StatusKind::Working => Style::new().fg(theme.warn),
            StatusKind::Info => Style::new().fg(theme.text),
        }
    }
}

fn prompt_hint(kind: &PromptKind) -> &'static str {
    match kind {
        PromptKind::Search => " type to filter · Enter apply · Esc cancel · Ctrl-u clear line",
        PromptKind::Command => {
            " Enter run · Esc cancel · ↑/↓ history · :help for commands · :w save · :scan rescan"
        }
        PromptKind::AddFolder => " Enter add the folder and scan · ~ is expanded · Esc cancel",
        PromptKind::NewPlaylist { .. } => " Enter create the playlist · Esc cancel",
        PromptKind::RenamePlaylist { .. } => " Enter rename · Esc cancel",
    }
}

fn draw_prompt(frame: &mut Frame, app: &App, prompt: &crate::app::PromptState, area: Rect) {
    let theme = app.theme;
    let prefix = match prompt.kind {
        PromptKind::Search => "/",
        PromptKind::Command => ":",
        PromptKind::AddFolder => "+",
        PromptKind::NewPlaylist { .. } => "+",
        PromptKind::RenamePlaylist { .. } => "~",
    };
    let before = &prompt.value[..prompt.cursor];
    let after = &prompt.value[prompt.cursor..];
    let mut next = after.chars().next();
    if next.is_none() {
        next = Some(' ');
    }
    let next_len = next.map(char::len_utf8).unwrap_or(0);
    let rest = after.get(next_len..).unwrap_or("");
    let cursor_char = next.unwrap_or(' ').to_string();

    let line = Line::from(vec![
        Span::styled(
            format!(" {prefix} "),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(before.to_string()),
        Span::styled(
            cursor_char,
            Style::new().fg(theme.text).add_modifier(Modifier::REVERSED),
        ),
        Span::raw(rest.to_string()),
        Span::styled(format!("   {}", prompt.title), theme.muted()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

fn draw_overlay(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    match app.overlay.as_ref() {
        None => {}
        Some(Overlay::Help(state)) => {
            let rect = popup(area, 84, 86);
            let lines = help_lines(&theme);
            draw_scroll(
                frame,
                rect,
                &theme,
                " Key map ".to_string(),
                lines,
                state.offset,
                " j/k scroll · Ctrl-d/u page · g/G top/bottom · Esc close ",
            );
        }
        Some(Overlay::Folders(state)) => {
            let rect = list_popup(area, 70, app.folders.folders.len(), 52);
            draw_folders_overlay(frame, rect, app, state.selected);
        }
        Some(Overlay::Info(state)) => {
            let rect = popup(area, 86, 86);
            if let Some(track) = app
                .library
                .read()
                .ok()
                .and_then(|library| library.find(&state.id).cloned())
            {
                let lines = info_lines(&theme, &track);
                draw_scroll(
                    frame,
                    rect,
                    &theme,
                    format!(" Track {} ", track.id),
                    lines,
                    state.offset,
                    " j/k scroll · Ctrl-d/u page · g/G top/bottom · Esc close ",
                );
            } else {
                draw_scroll(
                    frame,
                    rect,
                    &theme,
                    " Track ".to_string(),
                    vec![Line::from(Span::styled("Track not found", theme.error()))],
                    0,
                    " Esc close ",
                );
            }
        }
        Some(Overlay::Analysis(state)) => {
            let rect = popup(area, 88, 86);
            let lines = analysis_lines(&theme, state.state.as_ref(), &state.log);
            draw_scroll(
                frame,
                rect,
                &theme,
                " Analysis ".to_string(),
                lines,
                state.offset,
                " r refresh · j/k scroll · Ctrl-d/u page · Esc close ",
            );
        }
        Some(Overlay::PlaylistPicker(state)) => {
            let rect = list_popup(area, 58, app.playlists.names().len(), 62);
            draw_picker(frame, rect, app, state.selected, state.ids.len());
        }
        Some(Overlay::Confirm(state)) => {
            let rect = fixed_popup(area, 66, 9);
            frame.render_widget(Clear, rect);
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled(state.question.clone(), theme.strong())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("[y]", Style::new().fg(theme.good)),
                    Span::raw(" yes      "),
                    Span::styled("[n]", Style::new().fg(theme.error)),
                    Span::raw(" no"),
                ]),
            ];
            let block = panel(&theme, " Confirm ").style(Style::new().bg(Color::Reset));
            frame.render_widget(
                Paragraph::new(Text::from(lines))
                    .block(block)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false }),
                rect,
            );
        }
    }
}

fn draw_scroll(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: String,
    lines: Vec<Line<'static>>,
    offset: usize,
    footer: &str,
) {
    frame.render_widget(Clear, area);
    let inner_height = area.height.saturating_sub(2) as usize;
    let max_offset = lines.len().saturating_sub(inner_height);
    let offset = offset.min(max_offset).min(u16::MAX as usize) as u16;
    let block = panel(theme, title)
        .title_bottom(Line::from(Span::styled(footer.to_string(), theme.muted())))
        .padding(ratatui::widgets::Padding::new(1, 1, 0, 0));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .scroll((offset, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_folders_overlay(frame: &mut Frame, area: Rect, app: &App, selected: usize) {
    let theme = app.theme;
    frame.render_widget(Clear, area);
    let library = match app.library.read() {
        Ok(library) => library,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut items: Vec<ListItem> = Vec::new();
    for folder in &app.folders.folders {
        let count = folder_track_count(&library, folder);
        let exists = Path::new(folder).is_dir();
        let marker = if exists {
            Span::styled("● ", Style::new().fg(theme.good))
        } else {
            Span::styled("○ ", Style::new().fg(theme.error))
        };
        items.push(ListItem::new(Line::from(vec![
            marker,
            Span::raw(display_folder(folder)),
            Span::styled(format!("  {count} tracks"), theme.muted()),
        ])));
    }
    let title = format!(
        " Music folders · watching {} ",
        if app.folders.watch_enabled {
            "on"
        } else {
            "off"
        }
    );
    let block = panel(&theme, title)
        .title_bottom(Line::from(Span::styled(
            " a add · d del · w watch · Enter scan · r all · Esc close ",
            theme.muted(),
        )))
        .padding(ratatui::widgets::Padding::new(1, 1, 0, 0));
    if items.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(""),
                Line::from(Span::styled("No music folders connected", theme.strong())),
                Line::from(""),
                Line::from(Span::styled(
                    "Press a to add a folder, or o from any view",
                    theme.muted(),
                )),
            ]))
            .alignment(Alignment::Center),
            inner,
        );
        return;
    }
    let selected = selected.min(items.len().saturating_sub(1));
    let list = List::new(items)
        .block(block)
        .highlight_style(theme.selection);
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_picker(frame: &mut Frame, area: Rect, app: &App, selected: usize, count: usize) {
    let theme = app.theme;
    frame.render_widget(Clear, area);
    let names = app.playlists.names();
    let items: Vec<ListItem> = names
        .iter()
        .map(|name| {
            ListItem::new(Line::from(vec![
                Span::raw(name.clone()),
                Span::styled(format!("  {}", app.playlists.count(name)), theme.muted()),
            ]))
        })
        .collect();
    let title = format!(" Add {count} track(s) to… ");
    let block = panel(&theme, title)
        .title_bottom(Line::from(Span::styled(
            " Enter add · a new playlist · Esc cancel ",
            theme.muted(),
        )))
        .padding(ratatui::widgets::Padding::new(1, 1, 0, 0));
    let selected = selected.min(items.len().saturating_sub(1));
    let list = List::new(items)
        .block(block)
        .highlight_style(theme.selection);
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

// ---------------------------------------------------------------------------
// Overlay content
// ---------------------------------------------------------------------------

fn help_lines(theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    macro_rules! section {
        ($title:expr) => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                $title.to_string(),
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            )));
        };
    }
    macro_rules! key {
        ($keys:expr, $description:expr) => {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<18}", $keys), theme.strong()),
                Span::styled($description.to_string(), theme.muted()),
            ]));
        };
    }

    lines.push(Line::from(Span::styled(
        "gbonds — terminal music player",
        Style::new().fg(theme.text).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "Local library, local recommendations, mpv playback.",
        theme.muted(),
    )));

    section!("NAVIGATION");
    key!("j / k, ↓ / ↑", "move down / up");
    key!("Ctrl-d / Ctrl-u", "page down / up");
    key!("gg / G", "jump to top / bottom");
    key!("h / l", "focus sidebar / content");
    key!("Tab / Shift-Tab", "next / previous view");
    key!("1 2 3 4", "Library, For You, Queue, Playlists");
    key!("Enter", "play selection / open playlist");
    key!("Esc", "clear search, return focus");
    key!("f", "jump to the playing track");

    section!("PLAYBACK");
    key!("Space", "play or pause");
    key!("> / <", "next / previous track");
    key!("] / [", "seek +5 / -5 seconds");
    key!("+ / -", "volume up / down");
    key!("M", "mute toggle");
    key!("s / S", "shuffle / repeat mode");
    key!("e / E", "queue track / queue visible list");
    key!("C", "clear the queue");
    key!(", / .", "seek -1 / +1 second");

    section!("LIBRARY AND RECOMMENDATIONS");
    key!("/", "search; n / N next / previous match");
    key!("F", "cycle sort order");
    key!("r", "similar tracks for the selection");
    key!("R", "start radio from the selection");
    key!("x", "remove from queue or playlist");
    key!("D", "remove track from the cache");
    key!("J / K", "move item down / up in queue or playlist");
    key!("i / I", "track details / analysis log");

    section!("PLAYLISTS");
    key!("a", "add selection to a playlist");
    key!("A", "add the visible list to a playlist");
    key!("a in Playlists", "create a playlist");
    key!("r in Playlists", "rename the playlist");
    key!("d in Playlists", "delete the playlist");
    key!("Enter", "play the playlist / selected track");

    section!("FOLDERS AND ANALYSIS");
    key!("o", "add a music folder");
    key!("O", "manage folders");
    key!("w", "toggle folder watching");
    key!(":scan", "import new or changed files");
    key!(":analyze", "analyze tracks missing audio features");
    key!(":analyze!", "re-analyze every track (slow)");

    section!("MOUSE");
    key!("wheel", "scroll the list viewport");
    key!("click", "select a track");
    key!("double-click", "play a track");

    section!("APPLICATION");
    key!(":", "command line");
    key!("t", "cycle theme: auto / dark / light");
    key!(":w", "save library, playlists and folders");
    key!(":reload", "reload shared data from disk");
    key!("q or ZZ", "quit");
    key!("?", "close this help");

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Data lives in its own gbonds cache directory.",
        theme.muted(),
    )));
    lines.push(Line::from(Span::styled(
        "An existing TUIMusicRecomendator cache is copied once on first launch.",
        theme.muted(),
    )));
    lines
}

fn info_lines(theme: &Theme, track: &Track) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        track.title.clone(),
        Style::new().fg(theme.text).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        track.artist.clone(),
        Style::new().fg(theme.accent),
    )));
    lines.push(Line::from(""));

    lines.push(small_label(theme, "ID", track.id.clone()));
    lines.push(small_label(
        theme,
        "Duration",
        format_time(track.duration_seconds()),
    ));
    lines.push(small_label(
        theme,
        "Genre",
        format!("{} · {}", track.display_genre(), track.genre_family()),
    ));
    lines.push(small_label(
        theme,
        "Rating",
        format!("{}/5", track.rating.clamp(0, 5)),
    ));
    lines.push(small_label(
        theme,
        "Album",
        track.album.clone().unwrap_or_else(|| "—".to_string()),
    ));
    lines.push(small_label(
        theme,
        "Year",
        track
            .year
            .map(|year| year.to_string())
            .unwrap_or_else(|| "—".to_string()),
    ));
    lines.push(small_label(
        theme,
        "Track no.",
        track
            .track_number
            .map(|value| value.to_string())
            .unwrap_or_else(|| "—".to_string()),
    ));
    lines.push(small_label(theme, "File size", file_size_label(track)));
    lines.push(small_label(
        theme,
        "File",
        truncate_chars(&display_folder(&track_path_label(track)), 110),
    ));
    lines.push(Line::from(""));

    match track.analysis.as_ref() {
        Some(analysis) => {
            lines.push(Line::from(Span::styled(
                "ANALYSIS",
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            )));
            lines.extend(analysis_value_lines(theme, analysis));
        }
        None => {
            lines.push(Line::from(Span::styled(
                "No audio analysis yet. Run :scan or :analyze.",
                theme.warn(),
            )));
        }
    }

    if !track.extra.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "EXTRA FIELDS",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        )));
        let mut keys: Vec<&String> = track.extra.keys().collect();
        keys.sort();
        for key in keys {
            if let Some(value) = track.extra.get(key) {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {key:<16}"), theme.muted()),
                    Span::raw(json_label(value)),
                ]));
            }
        }
    }
    lines
}

fn analysis_value_lines(theme: &Theme, analysis: &Analysis) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut keys: Vec<&String> = analysis.keys().collect();
    keys.sort();
    for key in keys {
        if let Some(value) = analysis.get(key) {
            if value.is_array() {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {key:<20}"), theme.muted()),
                    Span::styled(json_label(value), theme.muted()),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {key:<20}"), theme.muted()),
                    Span::raw(json_label(value)),
                ]));
            }
        }
    }
    lines
}

fn analysis_lines(
    theme: &Theme,
    state: Option<&serde_json::Map<String, serde_json::Value>>,
    log: &[String],
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "ANALYSIS STATE",
        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
    )));
    match state {
        Some(state) => {
            let priority = [
                "stage",
                "message",
                "processed",
                "total",
                "percent",
                "current",
                "current_name",
                "updated_at",
                "pid",
                "tracks",
                "edges",
                "failed",
                "elapsed",
                "error",
            ];
            for key in priority {
                if let Some(value) = state.get(key) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {key:<14}"), theme.muted()),
                        Span::raw(json_label(value)),
                    ]));
                }
            }
            let mut rest: Vec<&String> = state
                .keys()
                .filter(|key| !priority.contains(&key.as_str()))
                .collect();
            rest.sort();
            for key in rest {
                if let Some(value) = state.get(key) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {key:<14}"), theme.muted()),
                        Span::raw(json_label(value)),
                    ]));
                }
            }
        }
        None => {
            lines.push(Line::from(Span::styled(
                "No analysis_state.json yet.",
                theme.muted(),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("LOG · last {} lines", log.len()),
        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
    )));
    if log.is_empty() {
        lines.push(Line::from(Span::styled(
            "The analysis log is empty.",
            theme.muted(),
        )));
    } else {
        for entry in log {
            lines.push(Line::from(Span::styled(
                format!("  {entry}"),
                theme.muted(),
            )));
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// Overlay content
// ---------------------------------------------------------------------------
