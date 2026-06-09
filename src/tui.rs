use std::fs;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::{ConfigStore, RemiaftConfig};
use crate::i18n::{self, Language, Text};
use crate::{manifest, process};

pub async fn run(store: ConfigStore) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut app = App::new(store)?;

    loop {
        app.refresh_console();
        terminal.draw(|frame| draw(frame, &app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if app.handle_key(key).await? {
                    break;
                }
            }
        }
    }

    Ok(())
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn draw<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.terminal.draw(f)?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Debug, Clone)]
enum Mode {
    LanguageSelect,
    Normal,
    AddName,
    AddDir,
    AddJar,
    EditDir,
    EditJar,
    EditJavaArgs,
    EditServerArgs,
    Command,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MainView {
    Details,
    Console,
}

struct Draft {
    name: String,
    dir: String,
    jar: String,
}

struct App {
    store: ConfigStore,
    config: RemiaftConfig,
    selected: usize,
    mode: Mode,
    language: Language,
    input: String,
    draft: Draft,
    status: String,
    versions: Vec<String>,
    main_view: MainView,
    show_details: bool,
    console_server_id: Option<String>,
    console_lines: Vec<String>,
    console_end: Option<usize>,
    console_follow: bool,
}

impl App {
    fn new(store: ConfigStore) -> Result<Self> {
        let config = store.load()?;
        let saved_language = config.language.as_deref().and_then(Language::from_code);
        let language = saved_language.unwrap_or(Language::English);
        let mode = if saved_language.is_some() {
            Mode::Normal
        } else {
            Mode::LanguageSelect
        };
        Ok(Self {
            store,
            config,
            selected: 0,
            mode,
            language,
            input: String::new(),
            draft: Draft {
                name: String::new(),
                dir: String::new(),
                jar: "server.jar".to_string(),
            },
            status: i18n::text(language, Text::Help).to_string(),
            versions: Vec::new(),
            main_view: MainView::Details,
            show_details: true,
            console_server_id: None,
            console_lines: Vec::new(),
            console_end: None,
            console_follow: true,
        })
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }

        match self.mode {
            Mode::LanguageSelect => self.handle_language_key(key),
            Mode::Normal => self.handle_normal_key(key).await,
            _ => self.handle_input_key(key),
        }
    }

    fn handle_language_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char('1') | KeyCode::Char('e') | KeyCode::Char('E') => {
                self.set_language(Language::English)?;
            }
            KeyCode::Char('2') | KeyCode::Char('z') | KeyCode::Char('Z') => {
                self.set_language(Language::ChineseSimplified)?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_normal_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Down if self.main_view == MainView::Console => self.scroll_console(1),
            KeyCode::Up if self.main_view == MainView::Console => self.scroll_console(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('n') => {
                self.mode = Mode::AddName;
                self.input.clear();
                self.status = self.t(Text::ServerNamePrompt).to_string();
            }
            KeyCode::Char('d') => self.delete_selected()?,
            KeyCode::Char('s') => self.start_selected()?,
            KeyCode::Char('x') => self.stop_selected()?,
            KeyCode::Char('r') => {
                self.stop_selected()?;
                self.start_selected()?;
            }
            KeyCode::Char('a') => self.toggle_auto_restart()?,
            KeyCode::Char('p') => self.begin_edit_dir(),
            KeyCode::Char('j') => self.begin_edit_jar(),
            KeyCode::Char('e') => self.begin_edit_java_args(),
            KeyCode::Char('g') => self.begin_edit_server_args(),
            KeyCode::Char('c') => self.begin_command(),
            KeyCode::Char('i') => self.begin_command(),
            KeyCode::Char('o') => self.toggle_console(),
            KeyCode::Char('b') => self.toggle_details(),
            KeyCode::End => self.follow_console(),
            KeyCode::PageUp => self.scroll_console_page(-1),
            KeyCode::PageDown => self.scroll_console_page(1),
            KeyCode::Char('v') => self.fetch_versions().await,
            KeyCode::Char('l') => {
                self.mode = Mode::LanguageSelect;
                self.status = i18n::text(self.language, Text::LanguagePromptHint).to_string();
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.status = self.t(Text::Cancelled).to_string();
            }
            KeyCode::Enter => self.commit_input()?,
            KeyCode::Backspace | KeyCode::Char('h')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.code == KeyCode::Backspace =>
            {
                self.input.pop();
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input.push(ch);
            }
            _ => {}
        }
        Ok(false)
    }

    fn commit_input(&mut self) -> Result<()> {
        match self.mode {
            Mode::AddName => {
                self.draft.name = self.input.trim().to_string();
                self.input.clear();
                self.mode = Mode::AddDir;
                self.status = self.t(Text::ServerDirPrompt).to_string();
            }
            Mode::AddDir => {
                self.draft.dir = self.input.trim().to_string();
                self.input = self.draft.jar.clone();
                self.mode = Mode::AddJar;
                self.status = self.t(Text::ServerJarPrompt).to_string();
            }
            Mode::AddJar => {
                self.draft.jar = fallback(self.input.trim(), "server.jar").to_string();
                let name = fallback(&self.draft.name, "Minecraft Server").to_string();
                let dir = PathBuf::from(fallback(&self.draft.dir, "."));
                let jar = PathBuf::from(&self.draft.jar);
                self.config.add_server(name, dir, jar);
                self.save()?;
                self.selected = self.config.servers.len().saturating_sub(1);
                self.input.clear();
                self.mode = Mode::Normal;
                self.status = self.t(Text::ServerAdded).to_string();
            }
            Mode::EditDir => {
                let directory = PathBuf::from(self.input.trim());
                if let Some(server) = self.selected_mut() {
                    server.directory = directory;
                    self.save()?;
                    self.status = self.t(Text::DirectoryUpdated).to_string();
                }
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Mode::EditJar => {
                let jar_path = PathBuf::from(self.input.trim());
                if let Some(server) = self.selected_mut() {
                    server.jar_path = jar_path;
                    self.save()?;
                    self.status = self.t(Text::JarUpdated).to_string();
                }
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Mode::EditJavaArgs => {
                let parts = split_args(&self.input);
                if let Some(server) = self.selected_mut() {
                    server.java_args = parts;
                    self.save()?;
                    self.status = self.t(Text::JavaArgsUpdated).to_string();
                }
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Mode::EditServerArgs => {
                let parts = split_args(&self.input);
                if let Some(server) = self.selected_mut() {
                    server.server_args = parts;
                    self.save()?;
                    self.status = self.t(Text::ServerArgsUpdated).to_string();
                }
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Mode::Command => {
                let command = self.input.trim().to_string();
                if !command.is_empty() {
                    if let Some(server) = self.selected() {
                        process::append_command(&self.store, server, &command)?;
                        self.status = format!("{} {}", self.t(Text::SentCommand), server.name);
                    }
                }
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Mode::LanguageSelect | Mode::Normal => {}
        }
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.config.servers.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len.saturating_sub(1) as isize) as usize;
        if self.main_view == MainView::Console {
            self.reset_console_for_selection();
        }
    }

    fn selected(&self) -> Option<&crate::config::ServerConfig> {
        self.config.servers.get(self.selected)
    }

    fn selected_mut(&mut self) -> Option<&mut crate::config::ServerConfig> {
        self.config.servers.get_mut(self.selected)
    }

    fn delete_selected(&mut self) -> Result<()> {
        if self.config.servers.is_empty() {
            return Ok(());
        }
        let removed = self.config.servers.remove(self.selected);
        self.selected = self.selected.saturating_sub(1);
        self.save()?;
        self.status = format!("{} {}", self.t(Text::Deleted), removed.name);
        Ok(())
    }

    fn start_selected(&mut self) -> Result<()> {
        if let Some(server) = self.selected() {
            process::start_supervisor(&self.store, server)?;
            self.status = format!("{} {}", self.t(Text::Started), server.name);
        }
        Ok(())
    }

    fn stop_selected(&mut self) -> Result<()> {
        if let Some(server) = self.selected() {
            process::stop_server(&self.store, server)?;
            self.status = format!("{} {}", self.t(Text::Stopped), server.name);
        }
        Ok(())
    }

    fn toggle_auto_restart(&mut self) -> Result<()> {
        if let Some(server) = self.selected_mut() {
            server.auto_restart = !server.auto_restart;
            let enabled = server.auto_restart;
            self.save()?;
            self.status = format!(
                "{} {}",
                self.t(Text::AutoRestart),
                if enabled {
                    self.t(Text::Enabled)
                } else {
                    self.t(Text::Disabled)
                }
            );
        }
        Ok(())
    }

    fn begin_edit_dir(&mut self) {
        if let Some(server) = self.selected() {
            self.input = server.directory.to_string_lossy().to_string();
            self.mode = Mode::EditDir;
            self.status = self.t(Text::EditDirectory).to_string();
        }
    }

    fn begin_edit_jar(&mut self) {
        if let Some(server) = self.selected() {
            self.input = server.jar_path.to_string_lossy().to_string();
            self.mode = Mode::EditJar;
            self.status = self.t(Text::EditJar).to_string();
        }
    }

    fn begin_edit_java_args(&mut self) {
        if let Some(server) = self.selected() {
            self.input = server.java_args.join(" ");
            self.mode = Mode::EditJavaArgs;
            self.status = self.t(Text::EditJavaArgs).to_string();
        }
    }

    fn begin_edit_server_args(&mut self) {
        if let Some(server) = self.selected() {
            self.input = server.server_args.join(" ");
            self.mode = Mode::EditServerArgs;
            self.status = self.t(Text::EditServerArgs).to_string();
        }
    }

    fn begin_command(&mut self) {
        if self.selected().is_some() {
            self.input.clear();
            self.mode = Mode::Command;
            self.status = self.t(Text::SendCommand).to_string();
        }
    }

    fn toggle_console(&mut self) {
        match self.main_view {
            MainView::Details => {
                if self.selected().is_some() {
                    self.main_view = MainView::Console;
                    self.reset_console_for_selection();
                    self.status = self.t(Text::ConsoleFollow).to_string();
                }
            }
            MainView::Console => {
                self.main_view = MainView::Details;
                self.status = self.t(Text::Help).to_string();
            }
        }
    }

    fn toggle_details(&mut self) {
        self.show_details = !self.show_details;
        if self.show_details {
            self.status = self.t(Text::Help).to_string();
        } else {
            self.status = self.t(Text::DetailPanelHidden).to_string();
        }
    }

    fn follow_console(&mut self) {
        if self.main_view == MainView::Console {
            self.console_follow = true;
            self.console_end = None;
            self.status = self.t(Text::ConsoleFollow).to_string();
        }
    }

    fn scroll_console_page(&mut self, direction: isize) {
        if self.main_view != MainView::Console {
            return;
        }
        let delta = if direction < 0 { -10 } else { 10 };
        self.scroll_console(delta);
    }

    fn scroll_console(&mut self, delta: isize) {
        if self.main_view != MainView::Console {
            return;
        }
        if delta < 0 {
            let end = self.console_end.unwrap_or(self.console_lines.len());
            self.console_end = Some(end.saturating_sub(delta.unsigned_abs()).max(1));
            self.console_follow = false;
            self.status = self.t(Text::ConsolePaused).to_string();
        } else {
            let end = self.console_end.unwrap_or(self.console_lines.len());
            let next = end
                .saturating_add(delta as usize)
                .min(self.console_lines.len());
            if next >= self.console_lines.len() {
                self.console_follow = true;
                self.console_end = None;
                self.status = self.t(Text::ConsoleFollow).to_string();
            } else {
                self.console_end = Some(next.max(1));
            }
        }
    }

    fn reset_console_for_selection(&mut self) {
        self.console_server_id = self.selected().map(|server| server.id.clone());
        self.console_lines.clear();
        self.console_end = None;
        self.console_follow = true;
    }

    fn refresh_console(&mut self) {
        if self.main_view != MainView::Console {
            return;
        }
        let Some(server) = self.selected().cloned() else {
            self.console_lines.clear();
            return;
        };
        if self.console_server_id.as_deref() != Some(server.id.as_str()) {
            self.reset_console_for_selection();
        }
        let path = process::minecraft_log_path_for(&self.store, &server);
        let content = fs::read_to_string(path).unwrap_or_default();
        self.console_lines = content.lines().map(ToString::to_string).collect();
        if self.console_lines.len() > 5_000 {
            let keep_from = self.console_lines.len() - 5_000;
            self.console_lines.drain(..keep_from);
            if let Some(end) = self.console_end {
                self.console_end = Some(end.saturating_sub(keep_from).max(1));
            }
        }
        if self.console_follow {
            self.console_end = None;
        } else {
            let len = self.console_lines.len();
            self.console_end = self.console_end.map(|end| end.min(len).max(1));
        }
    }

    fn console_visible_lines(&self, height: usize) -> Vec<Line<'static>> {
        if self.console_lines.is_empty() {
            return vec![Line::from(self.t(Text::ConsoleEmpty).to_string())];
        }

        let end = if self.console_follow {
            self.console_lines.len()
        } else {
            self.console_end
                .unwrap_or(self.console_lines.len())
                .min(self.console_lines.len())
                .max(1)
        };
        let start = end.saturating_sub(height);
        self.console_lines[start..end]
            .iter()
            .map(|line| Line::from(line.clone()))
            .collect()
    }

    async fn fetch_versions(&mut self) {
        self.status = self.t(Text::FetchingVersions).to_string();
        match manifest::fetch_versions(12).await {
            Ok(versions) => {
                let server_label = self.t(Text::Server).to_string();
                let client_only_label = self.t(Text::ClientOnly).to_string();
                self.versions = versions
                    .into_iter()
                    .map(|version| {
                        let server = if version.server_url.is_some() {
                            server_label.as_str()
                        } else {
                            client_only_label.as_str()
                        };
                        format!("{} ({}, {})", version.id, version.kind, server)
                    })
                    .collect();
                self.status = self.t(Text::VersionsUpdated).to_string();
            }
            Err(err) => {
                self.status = format!("{}: {err}", self.t(Text::VersionFetchFailed));
            }
        }
    }

    fn set_language(&mut self, language: Language) -> Result<()> {
        self.language = language;
        self.config.language = Some(language.code().to_string());
        self.save()?;
        self.mode = Mode::Normal;
        self.status = format!(
            "{}: {}",
            self.t(Text::LanguageSaved),
            language.display_name()
        );
        Ok(())
    }

    fn t(&self, key: Text) -> &'static str {
        i18n::text(self.language, key)
    }

    fn save(&self) -> Result<()> {
        self.store.save(&self.config)
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if matches!(app.mode, Mode::LanguageSelect) {
        draw_language_select(frame, app, area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(area);

    let header = Paragraph::new(Line::from(vec![
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
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    let wide = chunks[1].width >= 120;
    let show_side_panel = wide && app.show_details;
    let body = if show_side_panel {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(32),
                Constraint::Percentage(48),
                Constraint::Percentage(20),
            ])
            .split(chunks[1])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(chunks[1])
    };
    draw_server_list(frame, app, body[0]);
    match app.main_view {
        MainView::Details => draw_detail(frame, app, body[1]),
        MainView::Console => draw_console(frame, app, body[1]),
    }
    if show_side_panel && body.len() > 2 {
        draw_quick_panel(frame, app, body[2]);
    }

    let footer = Paragraph::new(app.status.as_str())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.t(Text::Status)),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(footer, chunks[2]);

    if !matches!(app.mode, Mode::Normal | Mode::LanguageSelect) {
        draw_input(frame, app, centered_rect(70, 20, area));
    }
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
    let items = if app.config.servers.is_empty() {
        vec![ListItem::new(app.t(Text::NoServers))]
    } else {
        app.config
            .servers
            .iter()
            .map(|server| {
                let status = process::runtime_status(&app.store, server);
                let color = match status {
                    process::RuntimeStatus::Running => Color::Green,
                    process::RuntimeStatus::Stopped => Color::Gray,
                    process::RuntimeStatus::Stale => Color::Yellow,
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:<8}", status_label(app, status)),
                        Style::default().fg(color),
                    ),
                    Span::raw(format!(" {}", server.name)),
                ]))
            })
            .collect()
    };

    let mut state = ListState::default();
    if !app.config.servers.is_empty() {
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
            Line::from(format!("{}: {}", app.t(Text::Name), server.name)),
            Line::from(format!("{}: {}", app.t(Text::Id), server.id)),
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
                "{}: {}M - {}M",
                app.t(Text::Memory),
                server.min_memory_mb,
                server.max_memory_mb
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
            Line::from(format!(
                "{}: {}",
                app.t(Text::AutoRestartField),
                server.auto_restart
            )),
            Line::from(format!(
                "{}: {}s",
                app.t(Text::RestartDelay),
                server.restart_delay_secs
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
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_console(frame: &mut Frame, app: &App, area: Rect) {
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
    let height = area.height.saturating_sub(2).max(1) as usize;
    let lines = app.console_visible_lines(height);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_quick_panel(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        app.t(Text::SelectedServer),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));

    if let Some(server) = app.selected() {
        let status = process::runtime_status(&app.store, server);
        lines.push(Line::from(format!(
            "{}: {}",
            app.t(Text::Name),
            server.name
        )));
        lines.push(Line::from(format!(
            "{}: {}",
            app.t(Text::Jar),
            server.jar_path.display()
        )));
        lines.push(Line::from(format!(
            "{}: {}",
            app.t(Text::Directory),
            server.directory.display()
        )));
        lines.push(Line::from(format!(
            "{}: {}",
            app.t(Text::Status),
            status_label(app, status)
        )));
    } else {
        lines.push(Line::from(app.t(Text::NoServerSelected)));
    }

    lines.push(Line::from(""));
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
            Line::from("o  console/details"),
            Line::from("i  send command"),
            Line::from("b  side panel"),
            Line::from("End  follow output"),
            Line::from("s  start"),
            Line::from("x  stop"),
            Line::from("r  restart"),
            Line::from("c  console command"),
            Line::from("a  auto-restart"),
            Line::from("e  Java args"),
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
            Line::from("o  控制台/详情"),
            Line::from("i  发送命令"),
            Line::from("b  侧栏面板"),
            Line::from("End  跟随输出"),
            Line::from("s  启动"),
            Line::from("x  停止"),
            Line::from("r  重启"),
            Line::from("c  控制台命令"),
            Line::from("a  自动重启"),
            Line::from("e  Java 参数"),
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
    frame.render_widget(Clear, area);
    let title = match app.mode {
        Mode::AddName => app.t(Text::InputNewName),
        Mode::AddDir => app.t(Text::InputNewDirectory),
        Mode::AddJar => app.t(Text::InputJarPath),
        Mode::EditDir => app.t(Text::InputEditDirectory),
        Mode::EditJar => app.t(Text::InputEditJar),
        Mode::EditJavaArgs => app.t(Text::InputEditJavaArgs),
        Mode::EditServerArgs => app.t(Text::InputEditServerArgs),
        Mode::Command => app.t(Text::InputSendCommand),
        Mode::LanguageSelect | Mode::Normal => "",
    };
    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);
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

fn fallback<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.trim().is_empty() {
        default
    } else {
        value.trim()
    }
}

fn split_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escape = false;

    for ch in input.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }

        match ch {
            '\\' => escape = true,
            '"' | '\'' if quote == Some(ch) => quote = None,
            '"' | '\'' if quote.is_none() => quote = Some(ch),
            ch if ch.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if escape {
        current.push('\\');
    }
    if !current.is_empty() {
        args.push(current);
    }

    args
}
