use std::collections::{HashMap, HashSet};

use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::i18n::{Language, Text};
use crate::process;

use super::input::input_view;
use super::{App, Mode, PendingOp, TreeItem};

/// Height of the compact detail strip under the log, including its border.
const DETAIL_HEIGHT: u16 = 6;
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 76;
const WIDE_DETAIL_MIN_WIDTH: usize = 72;

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct StatusCounts {
    total: usize,
    running: usize,
    stopped: usize,
    stale: usize,
}

impl StatusCounts {
    fn add(&mut self, status: process::RuntimeStatus) {
        self.total += 1;
        match status {
            process::RuntimeStatus::Running => self.running += 1,
            process::RuntimeStatus::Stopped => self.stopped += 1,
            process::RuntimeStatus::Stale => self.stale += 1,
        }
    }
}

/// A render-local view of the App's periodically refreshed runtime cache.
/// Keeping it indexed here makes the header, group rows and detail strip use
/// one coherent set of values without doing any process or filesystem work.
struct RuntimeSnapshot<'a> {
    by_id: HashMap<&'a str, process::RuntimeStatus>,
    groups: HashMap<&'a str, GroupStats<'a>>,
    group_names: HashMap<&'a str, &'a str>,
    counts: StatusCounts,
}

#[derive(Default)]
struct GroupStats<'a> {
    counts: StatusCounts,
    server_ids: Vec<&'a str>,
}

impl<'a> RuntimeSnapshot<'a> {
    fn new(app: &'a App) -> Self {
        Self::from_config(&app.config, |server| app.runtime_status(server))
    }

    fn from_config(
        config: &'a crate::config::RemiaftConfig,
        mut runtime_status: impl FnMut(&crate::config::ServerConfig) -> process::RuntimeStatus,
    ) -> Self {
        let mut by_id = HashMap::with_capacity(config.servers.len());
        let mut groups: HashMap<&'a str, GroupStats<'a>> =
            HashMap::with_capacity(config.groups.len());
        let mut counts = StatusCounts::default();
        let parents = config
            .groups
            .iter()
            .map(|group| (group.id.as_str(), group.parent_id.as_deref()))
            .collect::<HashMap<_, _>>();
        let group_names = config
            .groups
            .iter()
            .map(|group| (group.id.as_str(), group.name.as_str()))
            .collect::<HashMap<_, _>>();
        let mut visited = HashSet::with_capacity(config.groups.len());
        for server in &config.servers {
            let status = runtime_status(server);
            by_id.insert(server.id.as_str(), status);
            counts.add(status);
            visited.clear();
            let mut group_id = server.group_id.as_deref();
            while let Some(id) = group_id {
                let Some(parent_id) = parents.get(id).copied() else {
                    break;
                };
                if !visited.insert(id) {
                    break;
                }
                let stats = groups.entry(id).or_default();
                stats.counts.add(status);
                stats.server_ids.push(server.id.as_str());
                group_id = parent_id;
            }
        }
        Self {
            by_id,
            groups,
            group_names,
            counts,
        }
    }

    fn status(&self, server_id: &str) -> process::RuntimeStatus {
        self.by_id
            .get(server_id)
            .copied()
            .unwrap_or(process::RuntimeStatus::Stopped)
    }

    fn group(&self, group_id: &str) -> (StatusCounts, &[&str]) {
        self.groups
            .get(group_id)
            .map(|stats| (stats.counts, stats.server_ids.as_slice()))
            .unwrap_or((StatusCounts::default(), &[]))
    }

    fn group_name(&self, group_id: &str) -> Option<&str> {
        self.group_names.get(group_id).copied()
    }
}

/// Screen regions shared between rendering and layout-dependent state (the
/// log wrap width must match the rectangle the log is drawn into).
pub(super) struct Regions {
    pub(super) header: Rect,
    pub(super) list: Rect,
    pub(super) log: Rect,
    pub(super) detail: Rect,
    pub(super) footer: Rect,
}

pub(super) fn compute_regions(area: Rect, show_details: bool) -> Regions {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let body = rows[1];

    let (list, right) = if body.width >= SIDE_BY_SIDE_MIN_WIDTH {
        let list_width = (body.width.saturating_mul(31) / 100).clamp(28, 44);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(list_width), Constraint::Min(1)])
            .split(body);
        (columns[0], columns[1])
    } else {
        let list_height = if body.height <= 6 {
            body.height / 2
        } else {
            (body.height.saturating_mul(2) / 5)
                .clamp(3, 10)
                .min(body.height.saturating_sub(3))
        };
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(list_height), Constraint::Min(1)])
            .split(body);
        (split[0], split[1])
    };

    let (log, detail) = if show_details && right.height >= DETAIL_HEIGHT + 4 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(DETAIL_HEIGHT)])
            .split(right);
        (chunks[0], chunks[1])
    } else {
        (right, Rect::new(right.x, right.bottom(), right.width, 0))
    };

    Regions {
        header: rows[0],
        list,
        log,
        detail,
        footer: rows[2],
    }
}

pub(super) fn draw(frame: &mut Frame, app: &App) -> Option<Position> {
    let area = frame.area();
    frame.render_widget(Clear, area);
    if matches!(app.mode, Mode::LanguageSelect) {
        draw_language_select(frame, app, area);
        return None;
    }

    let regions = compute_regions(area, app.show_details);
    let rows = app.visible_tree();
    let selected_item = selected_tree_item(&rows, app.selected);
    let runtime = RuntimeSnapshot::new(app);
    draw_header(frame, app, &runtime, regions.header);
    draw_server_list(frame, app, &runtime, &rows, regions.list);
    draw_log(frame, app, selected_item, regions.log);
    if regions.detail.height > 0 {
        draw_detail(frame, app, &runtime, selected_item, regions.detail);
    }
    draw_footer(frame, app, regions.footer);

    let cursor_position = (!matches!(app.mode, Mode::Normal | Mode::LanguageSelect))
        .then(|| draw_input(frame, app, centered_input_rect(area)))
        .flatten();
    if app.show_help {
        draw_help(frame, app, area);
    }
    cursor_position
}

fn selected_tree_item(rows: &[super::TreeRow], selected: usize) -> Option<&TreeItem> {
    rows.get(selected).map(|row| &row.item)
}

fn draw_header(frame: &mut Frame, app: &App, runtime: &RuntimeSnapshot<'_>, area: Rect) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let spans = header_spans(app, runtime.counts, area.width as usize);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let hints = app.t(Text::FooterHints);
    let width = area.width as usize;
    let hints_width = display_width(hints);
    let line = if width >= hints_width + 12 {
        let status_width = width - hints_width - 2;
        let status = truncate_end(&app.status, status_width);
        let padding = status_width.saturating_sub(display_width(&status));
        Line::from(vec![
            Span::styled(format!(" {status}"), Style::default().fg(Color::White)),
            Span::raw(" ".repeat(padding + 1)),
            Span::styled(hints, Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(Span::styled(
            format!(" {}", truncate_end(&app.status, width.saturating_sub(1))),
            Style::default().fg(Color::White),
        ))
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_language_select(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(app.t(Text::LanguagePromptTitle));
    let lines = vec![
        Line::from(app.t(Text::LanguagePromptBody)),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "1",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  English"),
        ]),
        Line::from(vec![
            Span::styled(
                "2",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  简体中文"),
        ]),
        Line::from(""),
        Line::from(app.t(Text::LanguagePromptHint)),
    ];
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, centered_rect(70, 45, area));
}

fn pending_label(app: &App, server_id: &str) -> Option<&'static str> {
    app.pending_op(server_id).map(|op| match op {
        PendingOp::Start => app.t(Text::OpStarting),
        PendingOp::Stop => app.t(Text::OpStopping),
        PendingOp::Restart => app.t(Text::OpRestarting),
    })
}

fn status_symbol(status: process::RuntimeStatus) -> (&'static str, Color) {
    match status {
        process::RuntimeStatus::Running => ("●", Color::Green),
        process::RuntimeStatus::Stopped => ("○", Color::DarkGray),
        process::RuntimeStatus::Stale => ("!", Color::Yellow),
    }
}

fn server_row(
    app: &App,
    server: &crate::config::ServerConfig,
    depth: usize,
    status: process::RuntimeStatus,
    width: usize,
) -> Line<'static> {
    let indent = "  ".repeat(depth);
    let marked = app.marked_servers.contains(&server.id);
    let pending = pending_label(app, &server.id);
    let (symbol, color) = if pending.is_some() {
        ("◐", Color::Yellow)
    } else {
        status_symbol(status)
    };
    let prefix_width = display_width(&indent) + 4;
    let available = width.saturating_sub(prefix_width);
    let suffix = pending
        .or_else(|| (width >= 34).then_some(status_label(app, status)))
        .filter(|label| available >= display_width(label) + 6);
    let suffix_width = suffix.map(|label| display_width(label) + 2).unwrap_or(0);
    let name_width = width.saturating_sub(prefix_width + suffix_width);

    let mut spans = vec![
        Span::raw(indent),
        Span::styled(
            if marked { "✓ " } else { "  " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{symbol} "), Style::default().fg(color)),
        Span::raw(truncate_end(&server.name, name_width)),
    ];
    if let Some(label) = suffix {
        spans.push(Span::styled(
            format!("  {label}"),
            Style::default()
                .fg(if pending.is_some() {
                    Color::Yellow
                } else {
                    Color::DarkGray
                })
                .add_modifier(if pending.is_some() {
                    Modifier::ITALIC
                } else {
                    Modifier::empty()
                }),
        ));
    }
    Line::from(spans)
}

fn group_row(
    name: &str,
    depth: usize,
    marked: bool,
    expanded: bool,
    counts: StatusCounts,
    width: usize,
) -> Line<'static> {
    let indent = "  ".repeat(depth);
    let arrow = if expanded { "▾" } else { "▸" };
    let wide_summary = width.saturating_sub(display_width(&indent)) >= 24;
    let summary_width = if wide_summary {
        display_width(&format!(
            "  ●{} ○{} !{}",
            counts.running, counts.stopped, counts.stale
        ))
    } else {
        display_width(&format!("  {}/{}", counts.running, counts.total))
    };
    let prefix_width = display_width(&indent) + 4;
    let name_width = width.saturating_sub(prefix_width + summary_width);
    let mut spans = vec![
        Span::raw(indent),
        Span::styled(
            if marked { "✓ " } else { "  " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{arrow} "), Style::default().fg(Color::Cyan)),
        Span::styled(
            truncate_end(name, name_width),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if wide_summary {
        spans.extend([
            Span::raw("  "),
            Span::styled(
                format!("●{}", counts.running),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!(" ○{}", counts.stopped),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!(" !{}", counts.stale),
                Style::default().fg(Color::Yellow),
            ),
        ]);
    } else {
        spans.push(Span::styled(
            format!("  {}/{}", counts.running, counts.total),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn draw_server_list(
    frame: &mut Frame,
    app: &App,
    runtime: &RuntimeSnapshot<'_>,
    rows: &[super::TreeRow],
    area: Rect,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let bordered = area.height >= 3 && area.width >= 4;
    let content_width = if bordered {
        area.width.saturating_sub(2) as usize
    } else {
        area.width as usize
    };
    let visible_height = if bordered {
        area.height.saturating_sub(2).max(1) as usize
    } else {
        area.height.max(1) as usize
    };
    let (visible_start, visible_end) = visible_row_range(rows.len(), app.selected, visible_height);
    let items = if rows.is_empty() {
        vec![ListItem::new(app.t(Text::NoServers))]
    } else {
        rows[visible_start..visible_end]
            .iter()
            .map(|row| match &row.item {
                TreeItem::Group(group_id) => {
                    let name = runtime.group_name(group_id).unwrap_or("group");
                    let (counts, ids) = runtime.group(group_id);
                    let marked =
                        !ids.is_empty() && ids.iter().all(|id| app.marked_servers.contains(*id));
                    ListItem::new(group_row(
                        name,
                        row.depth,
                        marked,
                        app.expanded_groups.contains(group_id),
                        counts,
                        content_width,
                    ))
                }
                TreeItem::Server(index) => {
                    let Some(server) = app.config.servers.get(*index) else {
                        return ListItem::new("");
                    };
                    ListItem::new(server_row(
                        app,
                        server,
                        row.depth,
                        runtime.status(&server.id),
                        content_width,
                    ))
                }
            })
            .collect()
    };

    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(
            app.selected
                .min(rows.len() - 1)
                .saturating_sub(visible_start),
        ));
    }

    let marked = app.marked_servers.len();
    let title = if marked > 0 {
        format!("{} [{marked}]", app.t(Text::Servers))
    } else {
        app.t(Text::Servers).to_string()
    };
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let list = if bordered {
        list.block(Block::default().borders(Borders::ALL).title(title))
    } else {
        list
    };
    frame.render_stateful_widget(list, area, &mut state);
}

/// Restricts row formatting to what the terminal can actually display. A
/// ratatui `List` normally receives every item and clips later, which makes a
/// redraw allocate and style hundreds of invisible rows on large installs.
fn visible_row_range(row_count: usize, selected: usize, height: usize) -> (usize, usize) {
    if row_count == 0 || height == 0 {
        return (0, 0);
    }
    let height = height.min(row_count);
    let selected = selected.min(row_count - 1);
    let start = selected.saturating_sub(height - 1).min(row_count - height);
    (start, start + height)
}

fn draw_log(frame: &mut Frame, app: &App, selected_item: Option<&TreeItem>, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let selected_server = selected_item.and_then(|item| match item {
        TreeItem::Server(index) => app.config.servers.get(*index),
        TreeItem::Group(_) => None,
    });
    let title = if let Some(server) = selected_server {
        let mode = if app.console_follow {
            app.t(Text::ConsoleFollow)
        } else {
            app.t(Text::ConsolePaused)
        };
        format!("{} - {} ({mode})", app.t(Text::ServerLog), server.name)
    } else {
        app.t(Text::ServerLog).to_string()
    };
    let bordered = area.height >= 3 && area.width >= 4;
    let height = if bordered {
        area.height.saturating_sub(2).max(1) as usize
    } else {
        area.height.max(1) as usize
    };
    let lines = app.console_visible_lines(height);
    let paragraph = if bordered {
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
    } else {
        Paragraph::new(lines).wrap(Wrap { trim: false })
    };
    frame.render_widget(paragraph, area);
}

fn draw_detail(
    frame: &mut Frame,
    app: &App,
    runtime: &RuntimeSnapshot<'_>,
    selected_item: Option<&TreeItem>,
    area: Rect,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let bordered = area.height >= 3 && area.width >= 4;
    let content_width = if bordered {
        area.width.saturating_sub(2) as usize
    } else {
        area.width as usize
    };
    let paragraph = Paragraph::new(detail_lines(app, runtime, selected_item, content_width))
        .wrap(Wrap { trim: false });
    let paragraph = if bordered {
        paragraph.block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.t(Text::Details)),
        )
    } else {
        paragraph
    };
    frame.render_widget(paragraph, area);
}

fn detail_lines(
    app: &App,
    runtime: &RuntimeSnapshot<'_>,
    selected_item: Option<&TreeItem>,
    width: usize,
) -> Vec<Line<'static>> {
    let selected_server = selected_item.and_then(|item| match item {
        TreeItem::Server(index) => app.config.servers.get(*index),
        TreeItem::Group(_) => None,
    });
    if let Some(server) = selected_server {
        return server_detail_lines(app, server, runtime.status(&server.id), width);
    }
    if let Some(TreeItem::Group(group_id)) = selected_item {
        let name = runtime.group_name(group_id).unwrap_or("group");
        let (counts, _) = runtime.group(group_id);
        return vec![
            Line::from(Span::styled(
                truncate_end(&format!("{}: {name}", app.t(Text::SelectedServer)), width),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(fit_spans(summary_spans(app, counts, false), width)),
            Line::from(truncate_end(
                match app.language {
                    Language::English => "Enter marks this group · F5/F6/F7 start/stop/restart",
                    Language::ChineseSimplified => "Enter 选中整个分组 · F5/F6/F7 启动/停止/重启",
                },
                width,
            )),
        ];
    }
    vec![
        Line::from(truncate_end(app.t(Text::AddServerHint), width)),
        Line::from(truncate_end(app.t(Text::CustomJarHint), width)),
    ]
}

fn server_detail_lines(
    app: &App,
    server: &crate::config::ServerConfig,
    status: process::RuntimeStatus,
    width: usize,
) -> Vec<Line<'static>> {
    let command = server
        .startup_command
        .clone()
        .unwrap_or_else(|| server.startup_command(&app.config.java_path));
    let directory = server.directory.to_string_lossy();
    let jar = server.jar_path.to_string_lossy();
    let java = server.java_bin(&app.config.java_path);
    let version = server.version.as_deref().unwrap_or("—");
    let memory = memory_range(server.min_memory_mb, server.max_memory_mb);
    let auto = if server.auto_restart {
        app.t(Text::On)
    } else {
        app.t(Text::Off)
    };

    if width >= WIDE_DETAIL_MIN_WIDTH {
        vec![
            detail_identity_line(app, server, status, Some(version), width),
            two_field_line(
                app.t(Text::Directory),
                &directory,
                app.t(Text::Jar),
                &jar,
                width,
                58,
            ),
            two_field_line(
                app.t(Text::JavaPath),
                java,
                app.t(Text::Memory),
                &memory,
                width,
                62,
            ),
            compact_command_line(app, auto, server.restart_delay_secs, &command, width),
        ]
    } else {
        vec![
            detail_identity_line(app, server, status, None, width),
            field_line(app.t(Text::Directory), &directory, width, true),
            compact_spec_line(app, version, &jar, &memory, width),
            compact_command_line(app, auto, server.restart_delay_secs, &command, width),
        ]
    }
}

fn header_spans(app: &App, counts: StatusCounts, width: usize) -> Vec<Span<'static>> {
    let compact = summary_spans(app, counts, false);
    let verbose = summary_spans(app, counts, true);
    let compact_width = spans_width(&compact);
    let full_brand_width = display_width(" remiaft ");
    let brand = if width > compact_width + full_brand_width {
        " remiaft "
    } else if width >= compact_width + 4 {
        " R "
    } else {
        ""
    };
    let brand_width = display_width(brand);
    let mut spans = if brand.is_empty() {
        Vec::new()
    } else {
        vec![
            Span::styled(
                brand,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]
    };
    let summary = if width >= brand_width + 1 + spans_width(&verbose) {
        verbose
    } else {
        compact
    };
    spans.extend(summary);

    if width >= 100 {
        let used = spans_width(&spans);
        let prefix = format!("  │ {}: ", app.t(Text::Config));
        let prefix_width = display_width(&prefix);
        let available = width.saturating_sub(used);
        if available >= prefix_width + 10 {
            spans.push(Span::styled(prefix, Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled(
                truncate_start(
                    &app.store.config_path().to_string_lossy(),
                    available - prefix_width,
                ),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    fit_spans(spans, width)
}

fn summary_spans(app: &App, counts: StatusCounts, verbose: bool) -> Vec<Span<'static>> {
    if verbose {
        vec![
            Span::styled(
                format!("{} {}", app.t(Text::Total), counts.total),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("  ● {} {}", app.t(Text::Running), counts.running),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!("  ○ {} {}", app.t(Text::StoppedState), counts.stopped),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("  ! {} {}", app.t(Text::Issues), counts.stale),
                Style::default().fg(Color::Yellow),
            ),
        ]
    } else {
        vec![
            Span::styled(
                format!("Σ{}", counts.total),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("  ●{}", counts.running),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!("  ○{}", counts.stopped),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("  !{}", counts.stale),
                Style::default().fg(Color::Yellow),
            ),
        ]
    }
}

fn detail_identity_line(
    app: &App,
    server: &crate::config::ServerConfig,
    status: process::RuntimeStatus,
    version: Option<&str>,
    width: usize,
) -> Line<'static> {
    let pending = pending_label(app, &server.id);
    let (symbol, color) = if pending.is_some() {
        ("◐", Color::Yellow)
    } else {
        status_symbol(status)
    };
    let state = pending.unwrap_or_else(|| status_label(app, status));
    let state_text = format!("{symbol} {state}");
    let state_width = display_width(&state_text);
    let version_text = version.map(|value| {
        truncate_end(
            &format!("{}: {value}", app.t(Text::Version)),
            (width / 3).min(24),
        )
    });
    let version_width = version_text.as_deref().map(display_width).unwrap_or(0);
    let separators = 2 + usize::from(version_text.is_some()) * 5;
    let name_width = width.saturating_sub(state_width + version_width + separators);
    let mut spans = vec![
        Span::styled(
            state_text,
            Style::default()
                .fg(color)
                .add_modifier(if pending.is_some() {
                    Modifier::ITALIC
                } else {
                    Modifier::BOLD
                }),
        ),
        Span::raw("  "),
        Span::styled(
            truncate_end(&server.name, name_width),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(version_text) = version_text {
        spans.push(separator_span());
        spans.push(Span::styled(
            version_text,
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(fit_spans(spans, width))
}

fn field_line(label: &str, value: &str, width: usize, middle: bool) -> Line<'static> {
    Line::from(field_spans(label, value, width, middle))
}

fn field_spans(label: &str, value: &str, width: usize, middle: bool) -> Vec<Span<'static>> {
    let prefix = format!("{label}: ");
    let prefix_width = display_width(&prefix);
    if prefix_width >= width {
        return vec![Span::styled(
            truncate_end(&prefix, width),
            Style::default().fg(Color::DarkGray),
        )];
    }
    let value_width = width - prefix_width;
    let value = if middle {
        truncate_middle(value, value_width)
    } else {
        truncate_end(value, value_width)
    };
    vec![
        Span::styled(prefix, Style::default().fg(Color::DarkGray)),
        Span::raw(value),
    ]
}

fn two_field_line(
    left_label: &str,
    left_value: &str,
    right_label: &str,
    right_value: &str,
    width: usize,
    left_percent: usize,
) -> Line<'static> {
    const SEPARATOR: &str = " │ ";
    let separator_width = display_width(SEPARATOR);
    let fields_width = width.saturating_sub(separator_width);
    let left_width = fields_width.saturating_mul(left_percent) / 100;
    let right_width = fields_width.saturating_sub(left_width);
    let mut left = field_spans(left_label, left_value, left_width, true);
    let padding = left_width.saturating_sub(spans_width(&left));
    if padding > 0 {
        left.push(Span::raw(" ".repeat(padding)));
    }
    left.push(Span::styled(
        SEPARATOR,
        Style::default().fg(Color::DarkGray),
    ));
    left.extend(field_spans(right_label, right_value, right_width, true));
    Line::from(fit_spans(left, width))
}

fn compact_spec_line(
    app: &App,
    version: &str,
    jar: &str,
    memory: &str,
    width: usize,
) -> Line<'static> {
    let jar_name = std::path::Path::new(jar)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(jar);
    let (version, jar, memory) = if width >= 56 {
        (
            format!("{}: {version}", app.t(Text::Version)),
            format!("{}: {jar_name}", app.t(Text::Jar)),
            format!("{}: {memory}", app.t(Text::Memory)),
        )
    } else {
        (
            format!("v{version}"),
            format!("J:{jar_name}"),
            format!("M:{memory}"),
        )
    };
    three_segment_line(&version, &jar, &memory, width)
}

fn three_segment_line(first: &str, second: &str, third: &str, width: usize) -> Line<'static> {
    const SEPARATOR: &str = " · ";
    let separators_width = display_width(SEPARATOR) * 2;
    let fields_width = width.saturating_sub(separators_width);
    let first_width = display_width(first)
        .min((fields_width / 3).max(3))
        .min(fields_width);
    let third_width = display_width(third)
        .min((fields_width / 3).max(5))
        .min(fields_width.saturating_sub(first_width));
    let second_width = fields_width.saturating_sub(first_width + third_width);
    Line::from(fit_spans(
        vec![
            Span::styled(
                truncate_end(first, first_width),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(SEPARATOR, Style::default().fg(Color::DarkGray)),
            Span::raw(truncate_end(second, second_width)),
            Span::styled(SEPARATOR, Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate_end(third, third_width),
                Style::default().fg(Color::LightBlue),
            ),
        ],
        width,
    ))
}

fn compact_command_line(
    app: &App,
    auto: &str,
    delay_secs: u64,
    command: &str,
    width: usize,
) -> Line<'static> {
    const SEPARATOR: &str = " │ ";
    let auto_enabled = auto == app.t(Text::On);
    let auto_label = if width >= WIDE_DETAIL_MIN_WIDTH {
        app.t(Text::AutoRestartField)
    } else {
        app.t(Text::AutoShort)
    };
    let auto = format!(
        "{auto_label}: {auto} ({} {delay_secs}s)",
        app.t(Text::RestartDelay)
    );
    let command_prefix = format!("{}: ", app.t(Text::CommandShort));
    let separator_width = display_width(SEPARATOR);
    let minimum_command_width = display_width(&command_prefix) + 3;
    let auto_width = display_width(&auto).min(
        width
            .saturating_sub(separator_width + minimum_command_width)
            .max(width.saturating_sub(separator_width) / 2),
    );
    let command_width = width.saturating_sub(auto_width + separator_width);
    let command_value_width = command_width.saturating_sub(display_width(&command_prefix));
    Line::from(fit_spans(
        vec![
            Span::styled(
                truncate_end(&auto, auto_width),
                Style::default().fg(if auto_enabled {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
            ),
            Span::styled(SEPARATOR, Style::default().fg(Color::DarkGray)),
            Span::styled(command_prefix, Style::default().fg(Color::DarkGray)),
            Span::raw(truncate_middle(command, command_value_width)),
        ],
        width,
    ))
}

fn separator_span() -> Span<'static> {
    Span::styled("  │  ", Style::default().fg(Color::DarkGray))
}

fn memory_range(min_mb: u32, max_mb: u32) -> String {
    if min_mb.is_multiple_of(1024) && max_mb.is_multiple_of(1024) {
        format!("{}–{} GiB", min_mb / 1024, max_mb / 1024)
    } else {
        format!("{min_mb}–{max_mb} MiB")
    }
}

fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn fit_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut remaining = width;
    let mut fitted = Vec::with_capacity(spans.len());
    for span in spans {
        if remaining == 0 {
            break;
        }
        let span_width = display_width(span.content.as_ref());
        if span_width <= remaining {
            remaining -= span_width;
            fitted.push(span);
        } else {
            fitted.push(Span::styled(
                truncate_end(span.content.as_ref(), remaining),
                span.style,
            ));
            break;
        }
    }
    fitted
}

fn truncate_end(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let mut result = take_prefix(value, max_width - 1);
    result.push('…');
    result
}

fn truncate_start(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    format!("…{}", take_suffix(value, max_width - 1))
}

fn truncate_middle(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let available = max_width - 1;
    let prefix_width = available * 2 / 5;
    let suffix_width = available - prefix_width;
    format!(
        "{}…{}",
        take_prefix(value, prefix_width),
        take_suffix(value, suffix_width)
    )
}

fn take_prefix(value: &str, max_width: usize) -> String {
    let mut width = 0;
    value
        .chars()
        .take_while(|ch| {
            let char_width = UnicodeWidthChar::width(*ch).unwrap_or(0);
            if width + char_width > max_width {
                false
            } else {
                width += char_width;
                true
            }
        })
        .collect()
}

fn take_suffix(value: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut chars = value
        .chars()
        .rev()
        .take_while(|ch| {
            let char_width = UnicodeWidthChar::width(*ch).unwrap_or(0);
            if width + char_width > max_width {
                false
            } else {
                width += char_width;
                true
            }
        })
        .collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let entries: &[(&str, &str)] = match app.language {
        Language::English => &[
            ("s / x / r", "start / stop / restart current"),
            ("F5 / F6 / F7", "start / stop / restart marked or group"),
            ("Enter", "mark server or whole group"),
            (
                "o",
                "attach native console (Ctrl-U detach, Ctrl-C interrupt)",
            ),
            ("c / i", "send a single console command"),
            ("n", "new server"),
            ("F2", "new group"),
            ("F3 / m", "move to group"),
            ("Left / Right", "collapse / expand group"),
            ("Ctrl+D + exact name", "delete server or group"),
            ("a", "toggle auto-restart"),
            ("u", "edit startup command"),
            ("p / j / y", "edit directory / jar / Java path"),
            ("e / g", "edit Java args / server args"),
            ("PgUp / PgDn / End", "scroll log / follow"),
            ("b", "show/hide details"),
            ("l", "language"),
            ("q / Esc", "quit"),
        ],
        Language::ChineseSimplified => &[
            ("s / x / r", "启动 / 停止 / 重启当前"),
            ("F5 / F6 / F7", "启动 / 停止 / 重启已选或分组"),
            ("Enter", "选中服务器或整个分组"),
            ("o", "进入原生控制台（Ctrl-U 脱离，Ctrl-C 中断）"),
            ("c / i", "发送单条控制台命令"),
            ("n", "新建服务器"),
            ("F2", "新建分组"),
            ("F3 / m", "移动到分组"),
            ("← / →", "折叠 / 展开分组"),
            ("Ctrl+D + 完整名称", "删除服务器或分组（二次确认）"),
            ("a", "切换自动重启"),
            ("u", "编辑启动命令"),
            ("p / j / y", "编辑目录 / jar / Java 路径"),
            ("e / g", "编辑 Java 参数 / 服务端参数"),
            ("PgUp / PgDn / End", "滚动日志 / 恢复跟随"),
            ("b", "显示/隐藏详情"),
            ("l", "语言"),
            ("q / Esc", "退出"),
        ],
    };

    let mut lines: Vec<Line> = entries
        .iter()
        .map(|(key, description)| {
            Line::from(vec![
                Span::styled(
                    format!("  {key:<18}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(description.to_string()),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  {}", app.t(Text::HelpHint)),
        Style::default().fg(Color::DarkGray),
    )));

    let height = (lines.len() as u16 + 2).min(area.height);
    let width = 64.min(area.width);
    let rect = centered_fixed_rect(width, height, area);
    frame.render_widget(Clear, rect);
    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(app.t(Text::Shortcuts)),
    );
    frame.render_widget(paragraph, rect);
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) -> Option<Position> {
    if area.height == 0 || area.width == 0 {
        return None;
    }
    frame.render_widget(Clear, area);
    let title = match app.mode {
        Mode::AddName => app.t(Text::InputNewName),
        Mode::AddDir => app.t(Text::InputNewDirectory),
        Mode::AddStartupCommand => app.t(Text::StartupCommand),
        Mode::EditDir => app.t(Text::InputEditDirectory),
        Mode::EditJar => app.t(Text::InputEditJar),
        Mode::EditJavaPath => app.t(Text::InputEditJavaPath),
        Mode::EditJavaArgs => app.t(Text::InputEditJavaArgs),
        Mode::EditStartupCommand => app.t(Text::StartupCommand),
        Mode::EditServerArgs => app.t(Text::InputEditServerArgs),
        Mode::AddGroup => match app.language {
            Language::English => "New group path",
            Language::ChineseSimplified => "新分组路径",
        },
        Mode::MoveToGroup => match app.language {
            Language::English => "Move to group",
            Language::ChineseSimplified => "移动到分组",
        },
        Mode::Command => app.t(Text::InputSendCommand),
        Mode::DeleteConfirm => match app.language {
            Language::English => "Confirm exact name",
            Language::ChineseSimplified => "输入完整名称确认删除",
        },
        Mode::LanguageSelect | Mode::Normal => "",
    };
    let bordered = area.height >= 3 && area.width >= 4;
    let input_width = if bordered {
        area.width.saturating_sub(2).max(1) as usize
    } else {
        area.width.max(1) as usize
    };
    let (visible_input, cursor_col) = input_view(&app.input, app.input_cursor, input_width);
    let input = if bordered {
        Paragraph::new(visible_input)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
    } else {
        Paragraph::new(visible_input).wrap(Wrap { trim: false })
    };
    frame.render_widget(input, area);
    let cursor_x = if bordered {
        area.x.saturating_add(1).saturating_add(cursor_col)
    } else {
        area.x.saturating_add(cursor_col)
    };
    let cursor_y = if bordered {
        area.y.saturating_add(1)
    } else {
        area.y
    };
    Some(Position::new(
        cursor_x.min(area.right().saturating_sub(1)),
        cursor_y.min(area.bottom().saturating_sub(1)),
    ))
}

fn status_label(app: &App, status: process::RuntimeStatus) -> &'static str {
    match status {
        process::RuntimeStatus::Running => app.t(Text::Running),
        process::RuntimeStatus::Stopped => app.t(Text::StoppedState),
        process::RuntimeStatus::Stale => app.t(Text::Stale),
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn centered_input_rect(r: Rect) -> Rect {
    if r.width == 0 || r.height == 0 {
        return Rect::new(r.x, r.y, 0, 0);
    }
    let width = if r.width >= 100 {
        r.width.saturating_mul(70) / 100
    } else {
        r.width.saturating_sub(2)
    }
    .clamp(1, r.width);
    let height = if r.height >= 12 { 5 } else { 3 }.min(r.height.max(1));
    centered_fixed_rect(width, height, r)
}

fn centered_fixed_rect(width: u16, height: u16, r: Rect) -> Rect {
    if r.width == 0 || r.height == 0 {
        return Rect::new(r.x, r.y, 0, 0);
    }
    let width = width.min(r.width).max(1);
    let height = height.min(r.height).max(1);
    Rect {
        x: r.x + r.width.saturating_sub(width) / 2,
        y: r.y + r.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RemiaftConfig, ServerConfig, ServerGroup};
    use std::path::PathBuf;

    fn server(id: &str, group_id: Option<&str>) -> ServerConfig {
        ServerConfig {
            id: id.to_string(),
            name: id.to_string(),
            group_id: group_id.map(str::to_string),
            directory: PathBuf::from("."),
            startup_command: None,
            jar_path: PathBuf::from("server.jar"),
            java_path: None,
            min_memory_mb: 1024,
            max_memory_mb: 4096,
            java_args: Vec::new(),
            server_args: vec!["nogui".to_string()],
            auto_restart: false,
            restart_delay_secs: 10,
            version: None,
        }
    }

    #[test]
    fn truncation_respects_terminal_cell_width() {
        let end = truncate_end("服务器-alpha-long", 9);
        let middle = truncate_middle("/srv/minecraft/生存服/server.jar", 18);
        let start = truncate_start("/very/long/config/path/config.toml", 16);

        assert!(display_width(&end) <= 9);
        assert!(end.ends_with('…'));
        assert!(display_width(&middle) <= 18);
        assert!(middle.starts_with("/srv"));
        assert!(middle.ends_with("server.jar"));
        assert!(display_width(&start) <= 16);
        assert!(start.starts_with('…'));
        assert!(start.ends_with("config.toml"));
    }

    #[test]
    fn layout_switches_between_columns_and_rows() {
        let wide = compute_regions(Rect::new(0, 0, 120, 30), true);
        assert_eq!(wide.list.x, 0);
        assert_eq!(wide.list.width, 37);
        assert_eq!(wide.log.x, 37);
        assert_eq!(wide.detail.height, DETAIL_HEIGHT);

        let narrow = compute_regions(Rect::new(0, 0, 60, 24), true);
        assert_eq!(narrow.list.width, 60);
        assert_eq!(narrow.list.height, 8);
        assert_eq!(narrow.log.y, narrow.list.bottom());
        assert_eq!(narrow.detail.height, DETAIL_HEIGHT);
    }

    #[test]
    fn short_windows_keep_log_space_and_hide_details() {
        let regions = compute_regions(Rect::new(0, 0, 50, 10), true);
        assert_eq!(regions.detail.height, 0);
        assert!(regions.log.height > 0);
    }

    #[test]
    fn memory_range_uses_compact_units_when_exact() {
        assert_eq!(memory_range(1024, 4096), "1–4 GiB");
        assert_eq!(memory_range(1536, 4096), "1536–4096 MiB");
    }

    #[test]
    fn runtime_snapshot_aggregates_nested_groups_once() {
        let config = RemiaftConfig {
            groups: vec![
                ServerGroup {
                    id: "parent".to_string(),
                    name: "Parent".to_string(),
                    parent_id: None,
                },
                ServerGroup {
                    id: "child".to_string(),
                    name: "Child".to_string(),
                    parent_id: Some("parent".to_string()),
                },
                ServerGroup {
                    id: "empty".to_string(),
                    name: "Empty".to_string(),
                    parent_id: None,
                },
            ],
            servers: vec![
                server("parent-server", Some("parent")),
                server("child-server", Some("child")),
                server("root-server", None),
            ],
            ..Default::default()
        };
        let mut status_lookups = 0;
        let runtime = RuntimeSnapshot::from_config(&config, |server| {
            status_lookups += 1;
            match server.id.as_str() {
                "parent-server" => process::RuntimeStatus::Running,
                "child-server" => process::RuntimeStatus::Stale,
                _ => process::RuntimeStatus::Stopped,
            }
        });

        assert_eq!(status_lookups, config.servers.len());
        assert_eq!(
            runtime.counts,
            StatusCounts {
                total: 3,
                running: 1,
                stopped: 1,
                stale: 1,
            }
        );
        let (parent, parent_ids) = runtime.group("parent");
        assert_eq!(runtime.group_name("parent"), Some("Parent"));
        assert_eq!(parent.total, 2);
        assert_eq!(parent.running, 1);
        assert_eq!(parent.stale, 1);
        assert_eq!(parent_ids, ["parent-server", "child-server"]);

        let (child, child_ids) = runtime.group("child");
        assert_eq!(child.total, 1);
        assert_eq!(child.stale, 1);
        assert_eq!(child_ids, ["child-server"]);

        let (empty, empty_ids) = runtime.group("empty");
        assert_eq!(empty, StatusCounts::default());
        assert!(empty_ids.is_empty());
    }

    #[test]
    fn selected_item_comes_from_the_rendered_tree_snapshot() {
        let rows = [
            super::super::TreeRow {
                depth: 0,
                item: TreeItem::Group("group".to_string()),
            },
            super::super::TreeRow {
                depth: 1,
                item: TreeItem::Server(7),
            },
        ];

        assert!(matches!(
            selected_tree_item(&rows, 0),
            Some(TreeItem::Group(id)) if id == "group"
        ));
        assert!(matches!(
            selected_tree_item(&rows, 1),
            Some(TreeItem::Server(7))
        ));
        assert!(selected_tree_item(&rows, 2).is_none());
    }

    #[test]
    fn list_only_formats_the_visible_window() {
        assert_eq!(visible_row_range(1_000, 0, 20), (0, 20));
        assert_eq!(visible_row_range(1_000, 19, 20), (0, 20));
        assert_eq!(visible_row_range(1_000, 20, 20), (1, 21));
        assert_eq!(visible_row_range(1_000, 999, 20), (980, 1_000));
        assert_eq!(visible_row_range(5, 99, 20), (0, 5));
        assert_eq!(visible_row_range(0, 0, 20), (0, 0));
    }
}
