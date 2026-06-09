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
use crate::{manifest, process};

pub async fn run(store: ConfigStore) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut app = App::new(store)?;

    loop {
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
    input: String,
    draft: Draft,
    status: String,
    versions: Vec<String>,
}

impl App {
    fn new(store: ConfigStore) -> Result<Self> {
        let config = store.load()?;
        Ok(Self {
            store,
            config,
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            draft: Draft {
                name: String::new(),
                dir: String::new(),
                jar: "server.jar".to_string(),
            },
            status: "n new | s start | x stop | c command | a auto-restart | e args | p path | j jar | v versions | d delete | q quit".to_string(),
            versions: Vec::new(),
        })
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }

        match self.mode {
            Mode::Normal => self.handle_normal_key(key).await,
            _ => self.handle_input_key(key),
        }
    }

    async fn handle_normal_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('n') => {
                self.mode = Mode::AddName;
                self.input.clear();
                self.status = "server name:".to_string();
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
            KeyCode::Char('v') => self.fetch_versions().await,
            _ => {}
        }
        Ok(false)
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.status = "cancelled".to_string();
            }
            KeyCode::Enter => self.commit_input()?,
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(ch) => self.input.push(ch),
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
                self.status = "server directory:".to_string();
            }
            Mode::AddDir => {
                self.draft.dir = self.input.trim().to_string();
                self.input = self.draft.jar.clone();
                self.mode = Mode::AddJar;
                self.status = "server jar path, relative to directory is ok:".to_string();
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
                self.status = "server added".to_string();
            }
            Mode::EditDir => {
                let directory = PathBuf::from(self.input.trim());
                if let Some(server) = self.selected_mut() {
                    server.directory = directory;
                    self.save()?;
                    self.status = "directory updated".to_string();
                }
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Mode::EditJar => {
                let jar_path = PathBuf::from(self.input.trim());
                if let Some(server) = self.selected_mut() {
                    server.jar_path = jar_path;
                    self.save()?;
                    self.status = "jar path updated".to_string();
                }
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Mode::EditJavaArgs => {
                let parts = split_args(&self.input);
                if let Some(server) = self.selected_mut() {
                    server.java_args = parts;
                    self.save()?;
                    self.status = "java args updated".to_string();
                }
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Mode::EditServerArgs => {
                let parts = split_args(&self.input);
                if let Some(server) = self.selected_mut() {
                    server.server_args = parts;
                    self.save()?;
                    self.status = "server args updated".to_string();
                }
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Mode::Command => {
                let command = self.input.trim().to_string();
                if !command.is_empty() {
                    if let Some(server) = self.selected() {
                        process::append_command(&self.store, server, &command)?;
                        self.status = format!("sent command to {}", server.name);
                    }
                }
                self.input.clear();
                self.mode = Mode::Normal;
            }
            Mode::Normal => {}
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
        self.status = format!("deleted {}", removed.name);
        Ok(())
    }

    fn start_selected(&mut self) -> Result<()> {
        if let Some(server) = self.selected() {
            process::start_supervisor(&self.store, server)?;
            self.status = format!("started {}", server.name);
        }
        Ok(())
    }

    fn stop_selected(&mut self) -> Result<()> {
        if let Some(server) = self.selected() {
            process::stop_server(&self.store, server)?;
            self.status = format!("stopped {}", server.name);
        }
        Ok(())
    }

    fn toggle_auto_restart(&mut self) -> Result<()> {
        if let Some(server) = self.selected_mut() {
            server.auto_restart = !server.auto_restart;
            let enabled = server.auto_restart;
            self.save()?;
            self.status = format!(
                "auto-restart {}",
                if enabled { "enabled" } else { "disabled" }
            );
        }
        Ok(())
    }

    fn begin_edit_dir(&mut self) {
        if let Some(server) = self.selected() {
            self.input = server.directory.to_string_lossy().to_string();
            self.mode = Mode::EditDir;
            self.status = "edit directory:".to_string();
        }
    }

    fn begin_edit_jar(&mut self) {
        if let Some(server) = self.selected() {
            self.input = server.jar_path.to_string_lossy().to_string();
            self.mode = Mode::EditJar;
            self.status = "edit jar path:".to_string();
        }
    }

    fn begin_edit_java_args(&mut self) {
        if let Some(server) = self.selected() {
            self.input = server.java_args.join(" ");
            self.mode = Mode::EditJavaArgs;
            self.status = "edit java args:".to_string();
        }
    }

    fn begin_edit_server_args(&mut self) {
        if let Some(server) = self.selected() {
            self.input = server.server_args.join(" ");
            self.mode = Mode::EditServerArgs;
            self.status = "edit server args:".to_string();
        }
    }

    fn begin_command(&mut self) {
        if self.selected().is_some() {
            self.input.clear();
            self.mode = Mode::Command;
            self.status = "send console command:".to_string();
        }
    }

    async fn fetch_versions(&mut self) {
        self.status = "fetching Mojang versions...".to_string();
        match manifest::fetch_versions(12).await {
            Ok(versions) => {
                self.versions = versions
                    .into_iter()
                    .map(|version| {
                        let server = if version.server_url.is_some() {
                            "server"
                        } else {
                            "client-only"
                        };
                        format!("{} ({}, {})", version.id, version.kind, server)
                    })
                    .collect();
                self.status = "versions updated".to_string();
            }
            Err(err) => {
                self.status = format!("version fetch failed: {err}");
            }
        }
    }

    fn save(&self) -> Result<()> {
        self.store.save(&self.config)
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
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
            "  config: {}",
            app.store.config_path().to_string_lossy()
        )),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);
    draw_server_list(frame, app, body[0]);
    draw_detail(frame, app, body[1]);

    let footer = Paragraph::new(app.status.as_str())
        .block(Block::default().borders(Borders::ALL).title("Status"))
        .wrap(Wrap { trim: true });
    frame.render_widget(footer, chunks[2]);

    if !matches!(app.mode, Mode::Normal) {
        draw_input(frame, app, centered_rect(70, 20, area));
    }
}

fn draw_server_list(frame: &mut Frame, app: &App, area: Rect) {
    let items = if app.config.servers.is_empty() {
        vec![ListItem::new("No servers. Press n to add one.")]
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
                    Span::styled(format!("{:<8}", status.label()), Style::default().fg(color)),
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
        .block(Block::default().borders(Borders::ALL).title("Servers"))
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
            Line::from(format!("Name: {}", server.name)),
            Line::from(format!("Id: {}", server.id)),
            Line::from(format!("Directory: {}", server.directory.display())),
            Line::from(format!("Jar: {}", server.jar_path.display())),
            Line::from(format!(
                "Memory: {}M - {}M",
                server.min_memory_mb, server.max_memory_mb
            )),
            Line::from(format!("Java args: {}", server.java_args.join(" "))),
            Line::from(format!("Server args: {}", server.server_args.join(" "))),
            Line::from(format!("Auto restart: {}", server.auto_restart)),
            Line::from(format!("Restart delay: {}s", server.restart_delay_secs)),
            Line::from(""),
            Line::from("Recent Mojang versions:"),
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
            Line::from("Add a server with n."),
            Line::from("Use a vanilla server.jar, Paper, Fabric, Forge, or any custom jar."),
        ]
    };

    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("Details"))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Clear, area);
    let title = match app.mode {
        Mode::AddName => "New server name",
        Mode::AddDir => "New server directory",
        Mode::AddJar => "Server jar path",
        Mode::EditDir => "Edit directory",
        Mode::EditJar => "Edit jar path",
        Mode::EditJavaArgs => "Edit Java args",
        Mode::EditServerArgs => "Edit server args",
        Mode::Command => "Send command",
        Mode::Normal => "",
    };
    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);
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
