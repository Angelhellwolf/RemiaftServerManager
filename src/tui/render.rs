use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::i18n::{Language, Text};
use crate::process;

use super::input::input_view;
use super::{App, MainView, Mode, TreeItem};

pub(super) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    if matches!(app.mode, Mode::LanguageSelect) {
        draw_language_select(frame, app, area);
        return;
    }

    let header_height = if area.height >= 8 { 3 } else { 1 };
    let footer_height = if area.height >= 10 { 3 } else { 1 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(1),
            Constraint::Length(footer_height),
        ])
        .split(area);

    let header_text = Line::from(vec![
        Span::styled(
            "remiaft",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}: {}",
            app.t(Text::Config),
            app.store.config_path().to_string_lossy()
        )),
    ]);
    let header = if chunks[0].height >= 3 {
        Paragraph::new(header_text).block(Block::default().borders(Borders::ALL))
    } else {
        Paragraph::new(header_text)
    };
    frame.render_widget(header, chunks[0]);

    if app.main_view == MainView::Console {
        draw_console_workspace(frame, app, chunks[1]);
    } else {
        draw_manager_workspace(frame, app, chunks[1]);
    }

    let footer = if chunks[2].height >= 3 {
        Paragraph::new(app.status.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(app.t(Text::Status)),
            )
            .wrap(Wrap { trim: true })
    } else {
        Paragraph::new(app.status.as_str()).wrap(Wrap { trim: true })
    };
    frame.render_widget(footer, chunks[2]);

    if !matches!(app.mode, Mode::Normal | Mode::LanguageSelect) {
        draw_input(frame, app, centered_input_rect(area));
    }
}

fn draw_manager_workspace(frame: &mut Frame, app: &App, area: Rect) {
    if area.width >= 120 && app.show_details {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(32),
                Constraint::Percentage(48),
                Constraint::Percentage(20),
            ])
            .split(area);
        draw_server_list(frame, app, body[0]);
        draw_detail(frame, app, body[1]);
        draw_quick_panel(frame, app, body[2]);
    } else if area.width >= 72 {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(area);
        draw_server_list(frame, app, body[0]);
        draw_detail(frame, app, body[1]);
    } else {
        let list_height = if area.height >= 16 {
            (area.height / 3).clamp(4, 10)
        } else {
            (area.height / 2).max(3)
        };
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(list_height), Constraint::Min(1)])
            .split(area);
        draw_server_list(frame, app, body[0]);
        draw_detail(frame, app, body[1]);
    }
}

fn draw_console_workspace(frame: &mut Frame, app: &App, area: Rect) {
    if area.height < 6 {
        draw_console(frame, app, area);
        return;
    }
    let header_height = if area.height >= 12 {
        5
    } else if area.height >= 8 {
        3
    } else {
        0
    };
    if header_height == 0 {
        draw_console(frame, app, area);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_height), Constraint::Min(1)])
        .split(area);
    draw_console_server_header(frame, app, chunks[0]);
    draw_console(frame, app, chunks[1]);
}

fn draw_console_server_header(frame: &mut Frame, app: &App, area: Rect) {
    let lines = if let Some(server) = app.selected() {
        let status = process::runtime_status(&app.store, server);
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!("{}: {}", app.t(Text::Name), server.name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  {}: {}  {}: {}",
                app.t(Text::Status),
                status_label(app, status),
                app.t(Text::Id),
                server.id
            )),
        ])];
        if area.height >= 5 {
            lines.push(Line::from(format!(
                "{}: {}",
                app.t(Text::Directory),
                server.directory.display()
            )));
            lines.push(Line::from(format!(
                "{}: {}  |  Ctrl-U {}  |  Enter {}",
                app.t(Text::Jar),
                server.jar_path.display(),
                match app.language {
                    Language::English => "detach",
                    Language::ChineseSimplified => "脱离",
                },
                match app.language {
                    Language::English => "send",
                    Language::ChineseSimplified => "发送",
                }
            )));
        }
        lines
    } else {
        vec![Line::from(app.t(Text::NoServerSelected))]
    };

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.t(Text::SelectedServer)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_language_select(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_server_list(frame: &mut Frame, app: &App, area: Rect) {
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
                    let selected =
                        !ids.is_empty() && ids.iter().all(|id| app.marked_servers.contains(id));
                    let icon = if app.expanded_groups.contains(group_id) {
                        "[-]"
                    } else {
                        "[+]"
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw("  ".repeat(row.depth)),
                        Span::styled(
                            if selected { "[x] " } else { "[ ] " },
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(icon, Style::default().fg(Color::Yellow)),
                        Span::styled(
                            format!(" {name}"),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(" ({running}/{})", ids.len())),
                    ]))
                }
                TreeItem::Server(index) => {
                    let Some(server) = app.config.servers.get(*index) else {
                        return ListItem::new("");
                    };
                    let status = process::runtime_status(&app.store, server);
                    let color = match status {
                        process::RuntimeStatus::Running => Color::Green,
                        process::RuntimeStatus::Stopped => Color::Gray,
                        process::RuntimeStatus::Stale => Color::Yellow,
                    };
                    let selected = app.marked_servers.contains(&server.id);
                    ListItem::new(Line::from(vec![
                        Span::raw("  ".repeat(row.depth)),
                        Span::styled(
                            if selected { "[x] " } else { "[ ] " },
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            format!("{:<8}", status_label(app, status)),
                            Style::default().fg(color),
                        ),
                        Span::raw(format!(" {}", server.name)),
                    ]))
                }
            })
            .collect()
    };

    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(app.selected));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.t(Text::Servers)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let text = if let Some(server) = app.selected() {
        vec![
            Line::from(vec![
                Span::styled(
                    format!("{}: {}", app.t(Text::Name), server.name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}: {}", app.t(Text::Id), server.id)),
            ]),
            Line::from(format!(
                "{}: {}",
                app.t(Text::Directory),
                server.directory.display()
            )),
            Line::from(format!(
                "{}: {}",
                app.t(Text::Jar),
                server.jar_path.display()
            )),
            Line::from(format!(
                "{}: {} | {}: {}M-{}M | {}: {} | {}: {}s",
                app.t(Text::JavaPath),
                server.java_bin(&app.config.java_path),
                app.t(Text::Memory),
                server.min_memory_mb,
                server.max_memory_mb,
                app.t(Text::AutoRestartField),
                server.auto_restart,
                app.t(Text::RestartDelay),
                server.restart_delay_secs
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
                "{}: {}",
                app.t(Text::JavaArgs),
                server.java_args.join(" ")
            )),
            Line::from(format!(
                "{}: {}",
                app.t(Text::ServerArgs),
                server.server_args.join(" ")
            )),
            Line::from(""),
            Line::from(format!("{}:", app.t(Text::RecentVersions))),
        ]
        .into_iter()
        .chain(
            app.versions
                .iter()
                .map(|line| Line::from(format!("  {line}"))),
        )
        .collect()
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
            Line::from(format!("ID: {group_id}")),
            Line::from(match app.language {
                Language::English => format!("Servers: {} total, {running} running", ids.len()),
                Language::ChineseSimplified => {
                    format!("服务器：共 {} 个，运行中 {running} 个", ids.len())
                }
            }),
            Line::from(match app.language {
                Language::English => "Enter selects all servers in this group recursively.",
                Language::ChineseSimplified => "Enter 会递归选择该分组下的全部服务器。",
            }),
            Line::from(match app.language {
                Language::English => "F5/F6/F7 start, stop, or restart the group.",
                Language::ChineseSimplified => "F5/F6/F7 可启动、停止或重启该分组。",
            }),
        ]
    } else {
        vec![
            Line::from(app.t(Text::AddServerHint)),
            Line::from(app.t(Text::CustomJarHint)),
        ]
    };

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.t(Text::Details)),
        )
        .scroll((app.detail_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_console(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let input_height = if area.height >= 6 {
        3
    } else if area.height >= 3 {
        1
    } else {
        0
    };
    if input_height == 0 {
        draw_console_log(frame, app, area);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(input_height)])
        .split(area);
    draw_console_log(frame, app, chunks[0]);
    draw_console_input(frame, app, chunks[1]);
}

fn draw_console_log(frame: &mut Frame, app: &App, area: Rect) {
    let title = if let Some(server) = app.selected() {
        let mode = if app.console_follow {
            app.t(Text::ConsoleFollow)
        } else {
            app.t(Text::ConsolePaused)
        };
        format!("{} - {} ({mode})", app.t(Text::Console), server.name)
    } else {
        app.t(Text::Console).to_string()
    };
    let bordered = area.height >= 3 && area.width >= 4;
    let height = if bordered {
        area.height.saturating_sub(2).max(1) as usize
    } else {
        area.height.max(1) as usize
    };
    let width = if bordered {
        area.width.saturating_sub(2).max(1) as usize
    } else {
        area.width.max(1) as usize
    };
    let lines = app.console_visible_lines(height, width);
    let paragraph = if bordered {
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
    } else {
        Paragraph::new(lines).wrap(Wrap { trim: false })
    };
    frame.render_widget(paragraph, area);
}

fn draw_console_input(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let bordered = area.height >= 3 && area.width >= 4;
    let input_width = if bordered {
        area.width.saturating_sub(2).max(1) as usize
    } else {
        area.width.max(1) as usize
    };
    let (visible_input, cursor_col) =
        input_view(&app.console_input, app.console_cursor, input_width);
    let input = if bordered {
        Paragraph::new(visible_input)
            .block(Block::default().borders(Borders::ALL).title(format!(
                "{} - {}",
                app.t(Text::ConsoleInput),
                app.t(Text::ConsoleExitHint)
            )))
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

fn draw_quick_panel(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        app.t(Text::Shortcuts),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.extend(shortcut_lines(app));
    lines.push(Line::from(""));
    lines.push(Line::from(app.t(Text::ConsoleHint)));
    lines.push(Line::from(""));
    lines.push(Line::from(app.t(Text::ManagerExitHint)));

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.t(Text::QuickPanel)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn shortcut_lines(app: &App) -> Vec<Line<'static>> {
    match app.language {
        Language::English => vec![
            Line::from("n  new server"),
            Line::from("F2  new group"),
            Line::from("Enter  select item"),
            Line::from("Left/Right  fold group"),
            Line::from("F3/m  move to group"),
            Line::from("F5  start selected/group"),
            Line::from("F6  stop selected/group"),
            Line::from("F7  restart selected/group"),
            Line::from("o  console/details"),
            Line::from("i  send command"),
            Line::from("b  side panel"),
            Line::from("End  follow output"),
            Line::from("s  start current"),
            Line::from("x  stop current"),
            Line::from("r  restart current"),
            Line::from("c  console command"),
            Line::from("a  auto-restart"),
            Line::from("e  Java args"),
            Line::from("y  Java path"),
            Line::from("u  startup command"),
            Line::from("g  server args"),
            Line::from("p  directory"),
            Line::from("j  jar path"),
            Line::from("v  versions"),
            Line::from("l  language"),
            Line::from("d  delete"),
            Line::from("q  quit UI"),
        ],
        Language::ChineseSimplified => vec![
            Line::from("n  新建服务器"),
            Line::from("F2  新建分组"),
            Line::from("Enter  选择项目"),
            Line::from("←/→  折叠/展开分组"),
            Line::from("F3/m  移动到分组"),
            Line::from("F5  启动所选/分组"),
            Line::from("F6  停止所选/分组"),
            Line::from("F7  重启所选/分组"),
            Line::from("o  控制台/详情"),
            Line::from("i  发送命令"),
            Line::from("b  侧栏面板"),
            Line::from("End  跟随输出"),
            Line::from("s  启动当前"),
            Line::from("x  停止当前"),
            Line::from("r  重启当前"),
            Line::from("c  控制台命令"),
            Line::from("a  自动重启"),
            Line::from("e  Java 参数"),
            Line::from("y  Java 路径"),
            Line::from("u  启动命令"),
            Line::from("g  服务端参数"),
            Line::from("p  服务器目录"),
            Line::from("j  Jar 路径"),
            Line::from("v  版本列表"),
            Line::from("l  语言"),
            Line::from("d  删除"),
            Line::from("q  退出界面"),
        ],
    }
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
