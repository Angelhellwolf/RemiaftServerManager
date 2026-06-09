use std::fs;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    AddStartupCommand,
    EditDir,
    EditJar,
    EditJavaPath,
    EditJavaArgs,
    EditServerArgs,
    EditStartupCommand,
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
    startup_command: String,
}

struct App {
    store: ConfigStore,
    config: RemiaftConfig,
    selected: usize,
    mode: Mode,
    language: Language,
    input: String,
    input_cursor: usize,
    draft: Draft,
    status: String,
    versions: Vec<String>,
    main_view: MainView,
    show_details: bool,
    console_server_id: Option<String>,
    console_lines: Vec<String>,
    console_end: Option<usize>,
    console_follow: bool,
    console_input: String,
    console_cursor: usize,
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
            input_cursor: 0,
            draft: Draft {
                name: String::new(),
                dir: String::new(),
                startup_command: "java -Xms1024M -Xmx4096M -jar server.jar nogui".to_string(),
            },
            status: i18n::text(language, Text::Help).to_string(),
            versions: Vec::new(),
            main_view: MainView::Details,
            show_details: true,
            console_server_id: None,
            console_lines: Vec::new(),
            console_end: None,
            console_follow: true,
            console_input: String::new(),
            console_cursor: 0,
        })
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }

        match self.mode {
            Mode::LanguageSelect => self.handle_language_key(key),
            Mode::Normal if self.main_view == MainView::Console => {
                self.handle_console_key(key).await
            }
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
                self.clear_input();
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
            KeyCode::Char('u') => self.begin_edit_startup_command(),
            KeyCode::Char('y') => self.begin_edit_java_path(),
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

    async fn handle_console_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.leave_console();
            }
            KeyCode::Esc => {
                if self.console_input.is_empty() {
                    self.leave_console();
                } else {
                    self.clear_console_input();
                }
            }
            KeyCode::Enter => self.send_console_input()?,
            KeyCode::Left => move_cursor_left(&self.console_input, &mut self.console_cursor),
            KeyCode::Right => move_cursor_right(&self.console_input, &mut self.console_cursor),
            KeyCode::Home => self.console_cursor = 0,
            KeyCode::End if !self.console_input.is_empty() => {
                self.console_cursor = self.console_input.len()
            }
            KeyCode::Delete => delete_at_cursor(&mut self.console_input, self.console_cursor),
            KeyCode::Backspace | KeyCode::Char('h')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.code == KeyCode::Backspace =>
            {
                backspace_at_cursor(&mut self.console_input, &mut self.console_cursor);
            }
            KeyCode::Up => self.scroll_console(-1),
            KeyCode::Down => self.scroll_console(1),
            KeyCode::PageUp => self.scroll_console_page(-1),
            KeyCode::PageDown => self.scroll_console_page(1),
            KeyCode::End => self.follow_console(),
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_details()
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                insert_at_cursor(&mut self.console_input, &mut self.console_cursor, ch);
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.clear_input();
                self.status = self.t(Text::Cancelled).to_string();
            }
            KeyCode::Enter => self.commit_input()?,
            KeyCode::Left => move_cursor_left(&self.input, &mut self.input_cursor),
            KeyCode::Right => move_cursor_right(&self.input, &mut self.input_cursor),
            KeyCode::Up | KeyCode::Home => self.input_cursor = 0,
            KeyCode::Down | KeyCode::End => self.input_cursor = self.input.len(),
            KeyCode::Delete => delete_at_cursor(&mut self.input, self.input_cursor),
            KeyCode::Backspace | KeyCode::Char('h')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.code == KeyCode::Backspace =>
            {
                backspace_at_cursor(&mut self.input, &mut self.input_cursor);
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                insert_at_cursor(&mut self.input, &mut self.input_cursor, ch);
            }
            _ => {}
        }
        Ok(false)
    }

    fn commit_input(&mut self) -> Result<()> {
        match self.mode {
            Mode::AddName => {
                self.draft.name = self.input.trim().to_string();
                self.clear_input();
                self.mode = Mode::AddDir;
                self.status = self.t(Text::ServerDirPrompt).to_string();
            }
            Mode::AddDir => {
                self.draft.dir = self.input.trim().to_string();
                self.set_input(self.draft.startup_command.clone());
                self.mode = Mode::AddStartupCommand;
                self.status = self.t(Text::StartupCommandPrompt).to_string();
            }
            Mode::AddStartupCommand => {
                self.draft.startup_command = fallback(
                    self.input.trim(),
                    "java -Xms1024M -Xmx4096M -jar server.jar nogui",
                )
                .to_string();
                let name = fallback(&self.draft.name, "Minecraft Server").to_string();
                let dir = PathBuf::from(fallback(&self.draft.dir, "."));
                let parsed = parse_startup_command(&self.draft.startup_command, &dir);
                self.config.add_server(
                    name,
                    dir,
                    parsed
                        .jar_path
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("server.jar")),
                );
                if let Some(server) = self.config.servers.last_mut() {
                    apply_startup_command(server, parsed, &self.draft.startup_command);
                }
                self.save()?;
                self.selected = self.config.servers.len().saturating_sub(1);
                self.clear_input();
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
                self.clear_input();
                self.mode = Mode::Normal;
            }
            Mode::EditJar => {
                let jar_path = PathBuf::from(self.input.trim());
                if let Some(server) = self.selected_mut() {
                    server.jar_path = jar_path;
                    self.save()?;
                    self.status = self.t(Text::JarUpdated).to_string();
                }
                self.clear_input();
                self.mode = Mode::Normal;
            }
            Mode::EditJavaPath => {
                let java_path = self.input.trim().to_string();
                if let Some(server) = self.selected_mut() {
                    server.java_path = if java_path.is_empty() {
                        None
                    } else {
                        Some(java_path)
                    };
                    self.save()?;
                    self.status = self.t(Text::JavaArgsUpdated).to_string();
                }
                self.clear_input();
                self.mode = Mode::Normal;
            }
            Mode::EditJavaArgs => {
                let parts = split_args(&self.input);
                if let Some(server) = self.selected_mut() {
                    let normalized = normalize_startup_parts(parts);
                    server.java_path = normalized.java_path.or_else(|| server.java_path.clone());
                    server.jar_path = normalized
                        .jar_path
                        .unwrap_or_else(|| server.jar_path.clone());
                    if let Some(min_memory_mb) = normalized.min_memory_mb {
                        server.min_memory_mb = min_memory_mb;
                    }
                    if let Some(max_memory_mb) = normalized.max_memory_mb {
                        server.max_memory_mb = max_memory_mb;
                    }
                    server.java_args = normalized.java_args;
                    if !normalized.server_args.is_empty() {
                        server.server_args = normalized.server_args;
                    }
                    self.save()?;
                    self.status = if normalized.changed {
                        self.t(Text::StartupCommandNormalized).to_string()
                    } else {
                        self.t(Text::JavaArgsUpdated).to_string()
                    };
                }
                self.clear_input();
                self.mode = Mode::Normal;
            }
            Mode::EditStartupCommand => {
                let command = self.input.trim().to_string();
                let Some(selected) = self.selected_index() else {
                    self.clear_input();
                    self.mode = Mode::Normal;
                    return Ok(());
                };
                let directory = self.config.servers[selected].directory.clone();
                let parsed = parse_startup_command(&command, &directory);
                let server = &mut self.config.servers[selected];
                apply_startup_command(server, parsed, &command);
                self.save()?;
                self.status = self.t(Text::StartupCommandUpdated).to_string();
                self.clear_input();
                self.mode = Mode::Normal;
            }
            Mode::EditServerArgs => {
                let parts = split_args(&self.input);
                if let Some(server) = self.selected_mut() {
                    server.server_args = parts;
                    self.save()?;
                    self.status = self.t(Text::ServerArgsUpdated).to_string();
                }
                self.clear_input();
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
                self.clear_input();
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

    fn selected_index(&self) -> Option<usize> {
        (self.selected < self.config.servers.len()).then_some(self.selected)
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
            self.set_input(server.directory.to_string_lossy().to_string());
            self.mode = Mode::EditDir;
            self.status = self.t(Text::EditDirectory).to_string();
        }
    }

    fn begin_edit_jar(&mut self) {
        if let Some(server) = self.selected() {
            self.set_input(server.jar_path.to_string_lossy().to_string());
            self.mode = Mode::EditJar;
            self.status = self.t(Text::EditJar).to_string();
        }
    }

    fn begin_edit_java_path(&mut self) {
        let default_java = self.config.java_path.clone();
        if let Some(server) = self.selected() {
            self.set_input(server.java_path.clone().unwrap_or(default_java));
            self.mode = Mode::EditJavaPath;
            self.status = self.t(Text::EditJavaPath).to_string();
        }
    }

    fn begin_edit_java_args(&mut self) {
        if let Some(server) = self.selected() {
            self.set_input(server.java_args.join(" "));
            self.mode = Mode::EditJavaArgs;
            self.status = self.t(Text::EditJavaArgs).to_string();
        }
    }

    fn begin_edit_startup_command(&mut self) {
        if let Some(server) = self.selected() {
            self.set_input(
                server
                    .startup_command
                    .clone()
                    .unwrap_or_else(|| server.startup_command(&self.config.java_path)),
            );
            self.mode = Mode::EditStartupCommand;
            self.status = self.t(Text::EditStartupCommand).to_string();
        }
    }

    fn begin_edit_server_args(&mut self) {
        if let Some(server) = self.selected() {
            self.set_input(server.server_args.join(" "));
            self.mode = Mode::EditServerArgs;
            self.status = self.t(Text::EditServerArgs).to_string();
        }
    }

    fn begin_command(&mut self) {
        if self.selected().is_some() {
            self.clear_input();
            self.mode = Mode::Command;
            self.status = self.t(Text::SendCommand).to_string();
        }
    }

    fn send_console_input(&mut self) -> Result<()> {
        let command = self.console_input.trim().to_string();
        if command.is_empty() {
            return Ok(());
        }
        if let Some(server) = self.selected() {
            process::append_command(&self.store, server, &command)?;
            self.status = format!("{} {}", self.t(Text::SentCommand), server.name);
        }
        self.clear_console_input();
        self.follow_console();
        Ok(())
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
                self.leave_console();
            }
        }
    }

    fn leave_console(&mut self) {
        self.main_view = MainView::Details;
        self.clear_console_input();
        self.status = self.t(Text::Help).to_string();
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

    fn console_visible_lines(&self, height: usize, width: usize) -> Vec<Line<'static>> {
        if self.console_lines.is_empty() {
            return vec![Line::from(self.t(Text::ConsoleEmpty).to_string())];
        }

        let wrapped_lines = wrap_console_lines(&self.console_lines, width.max(1));
        let end = if self.console_follow {
            wrapped_lines.len()
        } else {
            self.console_end
                .unwrap_or(wrapped_lines.len())
                .min(wrapped_lines.len())
                .max(1)
        };
        let start = end.saturating_sub(height);
        wrapped_lines[start..end]
            .iter()
            .map(|line| ansi_to_line(line))
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

    fn set_input(&mut self, value: String) {
        self.input = value;
        self.input_cursor = self.input.len();
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
    }

    fn clear_console_input(&mut self) {
        self.console_input.clear();
        self.console_cursor = 0;
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

    if app.main_view == MainView::Console {
        draw_console_workspace(frame, app, chunks[1]);
    } else {
        draw_manager_workspace(frame, app, chunks[1]);
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

fn draw_manager_workspace(frame: &mut Frame, app: &App, area: Rect) {
    let wide = area.width >= 120;
    let show_side_panel = wide && app.show_details;
    let body = if show_side_panel {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(32),
                Constraint::Percentage(48),
                Constraint::Percentage(20),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(area)
    };
    draw_server_list(frame, app, body[0]);
    draw_detail(frame, app, body[1]);
    if show_side_panel && body.len() > 2 {
        draw_quick_panel(frame, app, body[2]);
    }
}

fn draw_console_workspace(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(6)])
        .split(area);
    draw_console_server_header(frame, app, chunks[0]);
    draw_console(frame, app, chunks[1]);
}

fn draw_console_server_header(frame: &mut Frame, app: &App, area: Rect) {
    let lines = if let Some(server) = app.selected() {
        let status = process::runtime_status(&app.store, server);
        vec![
            Line::from(vec![
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
            ]),
            Line::from(format!(
                "{}: {}",
                app.t(Text::Directory),
                server.directory.display()
            )),
            Line::from(format!(
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
            )),
        ]
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
                "{}: {}",
                app.t(Text::JavaPath),
                server.java_bin(&app.config.java_path)
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);
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
    let height = chunks[0].height.saturating_sub(2).max(1) as usize;
    let width = chunks[0].width.saturating_sub(2).max(1) as usize;
    let lines = app.console_visible_lines(height, width);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, chunks[0]);

    let input_width = chunks[1].width.saturating_sub(2).max(1) as usize;
    let (visible_input, cursor_col) =
        input_view(&app.console_input, app.console_cursor, input_width);
    let input = Paragraph::new(visible_input)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "{} - {}",
            app.t(Text::ConsoleInput),
            app.t(Text::ConsoleExitHint)
        )))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, chunks[1]);
    frame.set_cursor_position(Position::new(
        chunks[1].x.saturating_add(1).saturating_add(cursor_col),
        chunks[1].y.saturating_add(1),
    ));
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
        Mode::Command => app.t(Text::InputSendCommand),
        Mode::LanguageSelect | Mode::Normal => "",
    };
    let input_width = area.width.saturating_sub(2).max(1) as usize;
    let (visible_input, cursor_col) = input_view(&app.input, app.input_cursor, input_width);
    let input = Paragraph::new(visible_input)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);
    frame.set_cursor_position(Position::new(
        area.x.saturating_add(1).saturating_add(cursor_col),
        area.y.saturating_add(1),
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

fn fallback<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.trim().is_empty() {
        default
    } else {
        value.trim()
    }
}

fn input_view(value: &str, cursor: usize, width: usize) -> (String, u16) {
    let cursor = normalized_cursor(value, cursor);
    let width = width.max(1);
    let mut start = cursor;
    let mut used_width = 0;
    let before_cursor = &value[..cursor];

    for (index, ch) in before_cursor.char_indices().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used_width + ch_width > width.saturating_sub(1) {
            break;
        }
        start = index;
        used_width += ch_width;
    }

    let cursor_col = UnicodeWidthStr::width(&value[start..cursor]).min(width) as u16;
    let mut end = cursor;
    let mut total_width = cursor_col as usize;
    for (offset, ch) in value[cursor..].char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if total_width + ch_width > width {
            break;
        }
        end = cursor + offset + ch.len_utf8();
        total_width += ch_width;
    }

    (value[start..end].to_string(), cursor_col)
}

fn normalized_cursor(value: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(value.len());
    while cursor > 0 && !value.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn insert_at_cursor(value: &mut String, cursor: &mut usize, ch: char) {
    *cursor = normalized_cursor(value, *cursor);
    value.insert(*cursor, ch);
    *cursor += ch.len_utf8();
}

fn delete_at_cursor(value: &mut String, cursor: usize) {
    let cursor = normalized_cursor(value, cursor);
    if cursor >= value.len() {
        return;
    }
    let next = value[cursor..]
        .chars()
        .next()
        .map(|ch| cursor + ch.len_utf8())
        .unwrap_or(value.len());
    value.drain(cursor..next);
}

fn backspace_at_cursor(value: &mut String, cursor: &mut usize) {
    *cursor = normalized_cursor(value, *cursor);
    if *cursor == 0 {
        return;
    }
    let previous = value[..*cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0);
    value.drain(previous..*cursor);
    *cursor = previous;
}

fn move_cursor_left(value: &str, cursor: &mut usize) {
    *cursor = normalized_cursor(value, *cursor);
    if *cursor == 0 {
        return;
    }
    *cursor = value[..*cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0);
}

fn move_cursor_right(value: &str, cursor: &mut usize) {
    *cursor = normalized_cursor(value, *cursor);
    if *cursor >= value.len() {
        return;
    }
    if let Some(ch) = value[*cursor..].chars().next() {
        *cursor += ch.len_utf8();
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

struct NormalizedStartup {
    java_path: Option<String>,
    jar_path: Option<PathBuf>,
    min_memory_mb: Option<u32>,
    max_memory_mb: Option<u32>,
    java_args: Vec<String>,
    server_args: Vec<String>,
    changed: bool,
}

fn parse_startup_command(command: &str, _server_dir: &Path) -> NormalizedStartup {
    normalize_startup_parts(split_args(command))
}

fn apply_startup_command(
    server: &mut crate::config::ServerConfig,
    parsed: NormalizedStartup,
    command: &str,
) {
    if let Some(java_path) = parsed.java_path {
        server.java_path = Some(java_path);
    }
    if let Some(jar_path) = parsed.jar_path {
        server.jar_path = jar_path;
    }
    if let Some(min_memory_mb) = parsed.min_memory_mb {
        server.min_memory_mb = min_memory_mb;
    }
    if let Some(max_memory_mb) = parsed.max_memory_mb {
        server.max_memory_mb = max_memory_mb;
    }
    server.java_args = parsed.java_args;
    server.server_args = parsed.server_args;
    server.startup_command = Some(command.to_string());
}

fn normalize_startup_parts(parts: Vec<String>) -> NormalizedStartup {
    let Some(jar_index) = parts.iter().position(|part| part == "-jar") else {
        return NormalizedStartup {
            java_path: None,
            jar_path: None,
            min_memory_mb: None,
            max_memory_mb: None,
            java_args: parts,
            server_args: Vec::new(),
            changed: false,
        };
    };

    let java_path = parts
        .first()
        .filter(|part| looks_like_java_bin(part))
        .cloned();
    let java_arg_start = usize::from(java_path.is_some());
    let jar_path = parts.get(jar_index + 1).map(PathBuf::from);
    let mut min_memory_mb = None;
    let mut max_memory_mb = None;
    let mut java_args = Vec::new();
    for arg in &parts[java_arg_start..jar_index] {
        if let Some(value) = arg.strip_prefix("-Xms").and_then(parse_memory_mb) {
            min_memory_mb = Some(value);
        } else if let Some(value) = arg.strip_prefix("-Xmx").and_then(parse_memory_mb) {
            max_memory_mb = Some(value);
        } else {
            java_args.push(arg.clone());
        }
    }
    let server_args = parts.get(jar_index + 2..).unwrap_or(&[]).to_vec();

    NormalizedStartup {
        java_path,
        jar_path,
        min_memory_mb,
        max_memory_mb,
        java_args,
        server_args,
        changed: true,
    }
}

fn parse_memory_mb(value: &str) -> Option<u32> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (number, multiplier) = match value
        .chars()
        .last()
        .map(|ch| ch.to_ascii_uppercase())
        .unwrap_or('M')
    {
        'G' => (&value[..value.len() - 1], 1024),
        'M' => (&value[..value.len() - 1], 1),
        _ => (value, 1),
    };
    number.parse::<u32>().ok().map(|mb| mb * multiplier)
}

fn looks_like_java_bin(value: &str) -> bool {
    let name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value);
    name == "java" || name.starts_with("java")
}

fn wrap_console_lines(lines: &[String], width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for line in lines {
        wrap_console_line(line, width, &mut wrapped);
    }
    wrapped
}

fn wrap_console_line(line: &str, width: usize, output: &mut Vec<String>) {
    let mut current = String::new();
    let mut current_width = 0;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            current.push(ch);
            current.push(chars.next().unwrap_or('['));
            for next in chars.by_ref() {
                current.push(next);
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }

        if ch == '\r' {
            continue;
        }

        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width > 0 && current_width + ch_width > width {
            output.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }

    output.push(current);
}

fn ansi_to_line(input: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut style = Style::default();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            let mut sequence = String::new();
            let mut final_char = None;
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    final_char = Some(next);
                    break;
                }
                sequence.push(next);
            }

            if final_char == Some('m') {
                push_ansi_span(&mut spans, &mut text, style);
                apply_sgr(&mut style, &sequence);
            }
            continue;
        }

        if ch != '\r' {
            text.push(ch);
        }
    }

    push_ansi_span(&mut spans, &mut text, style);
    if spans.is_empty() {
        Line::from("")
    } else {
        Line::from(spans)
    }
}

fn push_ansi_span(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
    if !text.is_empty() {
        spans.push(Span::styled(std::mem::take(text), style));
    }
}

fn apply_sgr(style: &mut Style, sequence: &str) {
    let values = if sequence.trim().is_empty() {
        vec![0]
    } else {
        sequence
            .split(';')
            .map(|part| part.parse::<u16>().unwrap_or(0))
            .collect::<Vec<_>>()
    };

    let mut index = 0;
    while index < values.len() {
        match values[index] {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            30..=37 => *style = style.fg(ansi_color(values[index] - 30, false)),
            39 => *style = style.fg(Color::Reset),
            40..=47 => *style = style.bg(ansi_color(values[index] - 40, false)),
            49 => *style = style.bg(Color::Reset),
            90..=97 => *style = style.fg(ansi_color(values[index] - 90, true)),
            100..=107 => *style = style.bg(ansi_color(values[index] - 100, true)),
            38 | 48 => {
                if let Some((color, consumed)) = parse_extended_color(&values[index + 1..]) {
                    if values[index] == 38 {
                        *style = style.fg(color);
                    } else {
                        *style = style.bg(color);
                    }
                    index += consumed;
                }
            }
            _ => {}
        }
        index += 1;
    }
}

fn parse_extended_color(values: &[u16]) -> Option<(Color, usize)> {
    match values {
        [5, color, ..] => Some((Color::Indexed((*color).min(255) as u8), 2)),
        [2, red, green, blue, ..] => Some((
            Color::Rgb(
                (*red).min(255) as u8,
                (*green).min(255) as u8,
                (*blue).min(255) as u8,
            ),
            4,
        )),
        _ => None,
    }
}

fn ansi_color(code: u16, bright: bool) -> Color {
    match (code, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        (7, true) => Color::White,
        _ => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_startup_command() {
        let parsed = parse_startup_command(
            "java25 -Xms1G -Xmx4096M -Dfoo=bar -jar velocity.jar nogui",
            Path::new("."),
        );

        assert_eq!(parsed.java_path.as_deref(), Some("java25"));
        assert_eq!(parsed.min_memory_mb, Some(1024));
        assert_eq!(parsed.max_memory_mb, Some(4096));
        assert_eq!(parsed.jar_path.as_deref(), Some(Path::new("velocity.jar")));
        assert_eq!(parsed.java_args, vec!["-Dfoo=bar"]);
        assert_eq!(parsed.server_args, vec!["nogui"]);
    }

    #[test]
    fn parses_ansi_color_spans() {
        let line = ansi_to_line("\u{1b}[31mred\u{1b}[0m plain");
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content.as_ref(), "red");
        assert_eq!(line.spans[0].style.fg, Some(Color::Red));
        assert_eq!(line.spans[1].content.as_ref(), " plain");
    }

    #[test]
    fn wraps_long_console_lines_before_rendering() {
        let lines = vec![
            "[00:54:29 INFO]: bStats collects some basic information for plugin authors, like how many people use their plugin and their total player count. It's recommended to keep bStats enabled, but this text must continue."
                .to_string(),
        ];

        let wrapped = wrap_console_lines(&lines, 48);
        let joined = wrapped.join("");

        assert!(wrapped.len() > 1);
        assert!(joined.contains("but this text must continue."));
    }
}
