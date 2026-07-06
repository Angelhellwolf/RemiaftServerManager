use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::i18n::{Language, Text};
use crate::process;

use super::input::input_view;
use super::{App, Mode, PendingOp, TreeItem};

/// Height of the compact detail strip under the log, including its border.
const DETAIL_HEIGHT: u16 = 6;

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

    let (list, right) = if body.width >= 70 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(body);
        (columns[0], columns[1])
    } else {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(body.height / 3), Constraint::Min(1)])
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

pub(super) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    if matches!(app.mode, Mode::LanguageSelect) {
        draw_language_select(frame, app, area);
        return;
    }

    let regions = compute_regions(area, app.show_details);
    draw_header(frame, app, regions.header);
    draw_server_list(frame, app, regions.list);
    draw_log(frame, app, regions.log);
    if regions.detail.height > 0 {
        draw_detail(frame, app, regions.detail);
    }
    draw_footer(frame, app, regions.footer);

    if !matches!(app.mode, Mode::Normal | Mode::LanguageSelect) {
        draw_input(frame, app, centered_input_rect(area));
    }
    if app.show_help {
        draw_help(frame, app, area);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let mut spans = vec![
        Span::styled(
            " remiaft ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    let total = app.config.servers.len();
    let running = app
        .config
        .servers
        .iter()
        .filter(|server| {
            process::runtime_status(&app.store, server) == process::RuntimeStatus::Running
        })
        .count();
    spans.push(Span::styled(
        format!("{running}/{total} "),
        Style::default().fg(if running > 0 {
            Color::Green
        } else {
            Color::DarkGray
        }),
    ));
    spans.push(Span::styled(
        match app.language {
            Language::English => "running",
            Language::ChineseSimplified => "运行中",
        },
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(
        format!(
            "   {}: {}",
            app.t(Text::Config),
            app.store.config_path().to_string_lossy()
        ),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let hints = app.t(Text::FooterHints);
    let hints_width = hints.chars().count() as u16 + 2;
    let status_width = area.width.saturating_sub(hints_width);
    let line = Line::from(vec![
        Span::styled(
            format!(
                " {:<width$}",
                app.status,
                width = status_width.max(1) as usize - 1
            ),
            Style::default().fg(Color::White),
        ),
        Span::styled(hints, Style::default().fg(Color::DarkGray)),
    ]);
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

fn status_span(app: &App, server: &crate::config::ServerConfig) -> Span<'static> {
    if let Some(op) = app.pending_op(&server.id) {
        let label = match op {
            PendingOp::Start => app.t(Text::OpStarting),
            PendingOp::Stop => app.t(Text::OpStopping),
            PendingOp::Restart => app.t(Text::OpRestarting),
        };
        return Span::styled(
            format!("{label:<8}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        );
    }
    let status = process::runtime_status(&app.store, server);
    let (color, dot) = match status {
        process::RuntimeStatus::Running => (Color::Green, "●"),
        process::RuntimeStatus::Stopped => (Color::DarkGray, "○"),
        process::RuntimeStatus::Stale => (Color::Yellow, "◌"),
    };
    Span::styled(format!("{dot} "), Style::default().fg(color))
}

fn draw_server_list(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let rows = app.visible_tree();
    let items = if rows.is_empty() {
        vec![ListItem::new(app.t(Text::NoServers))]
    } else {
        rows.iter()
            .map(|row| match &row.item {
                TreeItem::Group(group_id) => {
                    let group = app.config.groups.iter().find(|group| &group.id == group_id);
                    let name = group.map(|group| group.name.as_str()).unwrap_or("group");
                    let ids = app.server_ids_in_group(group_id);
                    let running = ids
                        .iter()
                        .filter(|id| {
                            app.config
                                .servers
                                .iter()
                                .find(|server| &server.id == *id)
                                .map(|server| {
                                    process::runtime_status(&app.store, server)
                                        == process::RuntimeStatus::Running
                                })
                                .unwrap_or(false)
                        })
                        .count();
                    let marked =
                        !ids.is_empty() && ids.iter().all(|id| app.marked_servers.contains(id));
                    let arrow = if app.expanded_groups.contains(group_id) {
                        "▾"
                    } else {
                        "▸"
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw("  ".repeat(row.depth)),
                        Span::styled(
                            if marked { "* " } else { "  " },
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(format!("{arrow} "), Style::default().fg(Color::Yellow)),
                        Span::styled(
                            name.to_string(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  {running}/{}", ids.len()),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                }
                TreeItem::Server(index) => {
                    let Some(server) = app.config.servers.get(*index) else {
                        return ListItem::new("");
                    };
                    let marked = app.marked_servers.contains(&server.id);
                    ListItem::new(Line::from(vec![
                        Span::raw("  ".repeat(row.depth)),
                        Span::styled(
                            if marked { "* " } else { "  " },
                            Style::default().fg(Color::Cyan),
                        ),
                        status_span(app, server),
                        Span::raw(server.name.clone()),
                    ]))
                }
            })
            .collect()
    };

    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(app.selected));
    }

    let marked = app.marked_servers.len();
    let title = if marked > 0 {
        format!("{} [{marked}]", app.t(Text::Servers))
    } else {
        app.t(Text::Servers).to_string()
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_log(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let title = if let Some(server) = app.selected() {
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

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let paragraph = Paragraph::new(detail_lines(app))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.t(Text::Details)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn detail_lines(app: &App) -> Vec<Line<'static>> {
    if let Some(server) = app.selected() {
        let status = process::runtime_status(&app.store, server);
        let status_color = match status {
            process::RuntimeStatus::Running => Color::Green,
            process::RuntimeStatus::Stopped => Color::DarkGray,
            process::RuntimeStatus::Stale => Color::Yellow,
        };
        vec![
            Line::from(vec![
                Span::styled(
                    server.name.clone(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", status_label(app, status)),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    format!("  {}", server.id),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(format!(
                "{}: {}",
                app.t(Text::Directory),
                server.directory.display()
            )),
            Line::from(format!(
                "{}: {}",
                app.t(Text::StartupCommand),
                server
                    .startup_command
                    .clone()
                    .unwrap_or_else(|| server.startup_command(&app.config.java_path))
            )),
            Line::from(format!(
                "{}: {}  {}: {}M-{}M  {}: {}",
                app.t(Text::JavaPath),
                server.java_bin(&app.config.java_path),
                app.t(Text::Memory),
                server.min_memory_mb,
                server.max_memory_mb,
                app.t(Text::AutoRestartField),
                if server.auto_restart {
                    app.t(Text::Enabled)
                } else {
                    app.t(Text::Disabled)
                }
            )),
        ]
    } else if let Some(group_id) = app.selected_group_id() {
        let name = app
            .config
            .groups
            .iter()
            .find(|group| group.id == group_id)
            .map(|group| group.name.as_str())
            .unwrap_or("group");
        let ids = app.server_ids_in_group(&group_id);
        let running = ids
            .iter()
            .filter(|id| {
                app.config
                    .servers
                    .iter()
                    .find(|server| &server.id == *id)
                    .map(|server| {
                        process::runtime_status(&app.store, server)
                            == process::RuntimeStatus::Running
                    })
                    .unwrap_or(false)
            })
            .count();
        vec![
            Line::from(Span::styled(
                format!("{}: {name}", app.t(Text::SelectedServer)),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(match app.language {
                Language::English => format!("Servers: {} total, {running} running", ids.len()),
                Language::ChineseSimplified => {
                    format!("服务器：共 {} 个，运行中 {running} 个", ids.len())
                }
            }),
            Line::from(match app.language {
                Language::English => "Enter marks the whole group; F5/F6/F7 start/stop/restart it.",
                Language::ChineseSimplified => {
                    "Enter 选中整个分组；F5/F6/F7 启动/停止/重启该分组。"
                }
            }),
        ]
    } else {
        vec![
            Line::from(app.t(Text::AddServerHint)),
            Line::from(app.t(Text::CustomJarHint)),
        ]
    }
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
            ("d", "delete server or group"),
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
            ("d", "删除服务器或分组"),
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

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
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
    frame.set_cursor_position(Position::new(
        cursor_x.min(area.right().saturating_sub(1)),
        cursor_y.min(area.bottom().saturating_sub(1)),
    ));
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
    let width = width.min(r.width).max(1);
    let height = height.min(r.height).max(1);
    Rect {
        x: r.x + r.width.saturating_sub(width) / 2,
        y: r.y + r.height.saturating_sub(height) / 2,
        width,
        height,
    }
}
