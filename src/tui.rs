use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::config::{ConfigStore, RemiaftConfig};
use crate::i18n::{self, Language, Text};
use crate::{manifest, process};

mod console_log;
mod input;
mod render;
mod startup;
mod terminal;

use console_log::{ansi_to_line, wrap_console_lines};
use input::{
    backspace_at_cursor, complete_input_token, delete_at_cursor, fallback, insert_at_cursor,
    move_cursor_left, move_cursor_right,
};
use startup::{apply_startup_command, normalize_startup_parts, parse_startup_command, split_args};
use terminal::TerminalGuard;

pub async fn run(store: ConfigStore) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut app = App::new(store)?;

    loop {
        app.refresh_console();
        let size = terminal.size()?;
        app.update_console_layout(size);
        if app.take_screen_clear() {
            terminal.clear()?;
        }
        terminal.draw(|frame| render::draw(frame, &app))?;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) => {
                    if app.handle_key(key).await? {
                        break;
                    }
                }
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
            }
        }
    }

    Ok(())
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
    AddGroup,
    MoveToGroup,
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

#[derive(Debug, Clone)]
enum TreeItem {
    Group(String),
    Server(usize),
}

#[derive(Debug, Clone)]
struct TreeRow {
    depth: usize,
    item: TreeItem,
}

struct App {
    store: ConfigStore,
    config: RemiaftConfig,
    selected: usize,
    expanded_groups: HashSet<String>,
    marked_servers: HashSet<String>,
    mode: Mode,
    language: Language,
    input: String,
    input_cursor: usize,
    draft: Draft,
    status: String,
    versions: Vec<String>,
    main_view: MainView,
    show_details: bool,
    detail_scroll: u16,
    console_server_id: Option<String>,
    console_lines: Vec<String>,
    console_end: Option<usize>,
    console_follow: bool,
    console_wrap_width: usize,
    needs_screen_clear: bool,
}

impl App {
    fn new(store: ConfigStore) -> Result<Self> {
        let config = store.load()?;
        let expanded_groups = config.groups.iter().map(|group| group.id.clone()).collect();
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
            expanded_groups,
            marked_servers: HashSet::new(),
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
            detail_scroll: 0,
            console_server_id: None,
            console_lines: Vec::new(),
            console_end: None,
            console_follow: true,
            console_wrap_width: 120,
            needs_screen_clear: false,
        })
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(self.mode, Mode::Normal) && self.main_view == MainView::Console {
                self.interrupt_console_server()?;
                return Ok(false);
            }
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

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !matches!(self.mode, Mode::Normal) || self.main_view != MainView::Console {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_console(-3),
            MouseEventKind::ScrollDown => self.scroll_console(3),
            _ => {}
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
            KeyCode::F(2) => self.begin_add_group(),
            KeyCode::F(3) => self.begin_move_to_group(),
            KeyCode::F(5) => self.start_targets()?,
            KeyCode::F(6) => self.stop_targets()?,
            KeyCode::F(7) => self.restart_targets()?,
            KeyCode::Down if self.main_view == MainView::Console => self.scroll_console(1),
            KeyCode::Up if self.main_view == MainView::Console => self.scroll_console(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Left => self.collapse_selected_group(),
            KeyCode::Right => self.expand_selected_group(),
            KeyCode::Enter => self.toggle_mark_selected(),
            KeyCode::PageUp if self.main_view == MainView::Details => self.scroll_detail(-6),
            KeyCode::PageDown if self.main_view == MainView::Details => self.scroll_detail(6),
            KeyCode::Char('n') => {
                self.mode = Mode::AddName;
                self.clear_input();
                self.status = self.t(Text::ServerNamePrompt).to_string();
            }
            KeyCode::Char('d') => self.delete_selected()?,
            KeyCode::Char('s') => self.start_selected()?,
            KeyCode::Char('x') => self.stop_selected()?,
            KeyCode::Char('r') => self.restart_selected()?,
            KeyCode::Char('a') => self.toggle_auto_restart()?,
            KeyCode::Char('m') => self.begin_move_to_group(),
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
                self.leave_console();
            }
            KeyCode::Enter => self.send_console_terminal_input("\r").map(|_| ())?,
            KeyCode::Left => self.send_console_terminal_input("\u{1b}[D").map(|_| ())?,
            KeyCode::Right => self.send_console_terminal_input("\u{1b}[C").map(|_| ())?,
            KeyCode::Up => self.send_console_terminal_input("\u{1b}[A").map(|_| ())?,
            KeyCode::Down => self.send_console_terminal_input("\u{1b}[B").map(|_| ())?,
            KeyCode::Home => self.send_console_terminal_input("\u{1b}[H").map(|_| ())?,
            KeyCode::End => self.send_console_terminal_input("\u{1b}[F").map(|_| ())?,
            KeyCode::Delete => self.send_console_terminal_input("\u{1b}[3~").map(|_| ())?,
            KeyCode::Backspace | KeyCode::Char('h')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.code == KeyCode::Backspace =>
            {
                self.send_console_terminal_input("\u{7f}")?;
            }
            KeyCode::PageUp => self.scroll_console_page(-1),
            KeyCode::PageDown => self.scroll_console_page(1),
            KeyCode::Tab => self.send_console_terminal_input("\t").map(|_| ())?,
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_details()
            }
            KeyCode::Char(ch)
                if (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                    && process::is_allowed_console_char(ch) =>
            {
                let mut encoded = [0; 4];
                self.send_console_terminal_input(ch.encode_utf8(&mut encoded))?;
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
            KeyCode::Tab => self.complete_prompt_input(),
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
            Mode::AddGroup => {
                let path = self.input.trim().to_string();
                if !path.is_empty() {
                    if let Some(group_id) = self.config.ensure_group_path(&path) {
                        self.expanded_groups.insert(group_id);
                    }
                    self.save()?;
                    self.status = match self.language {
                        Language::English => format!("group created: {path}"),
                        Language::ChineseSimplified => format!("分组已创建：{path}"),
                    };
                }
                self.clear_input();
                self.mode = Mode::Normal;
            }
            Mode::MoveToGroup => {
                let path = self.input.trim().to_string();
                let target_group_id = if path.is_empty() {
                    None
                } else {
                    self.config.ensure_group_path(&path)
                };
                let ids = self.marked_or_selected_server_ids();
                let count = ids.len();
                for server in &mut self.config.servers {
                    if ids.contains(&server.id) {
                        server.group_id = target_group_id.clone();
                    }
                }
                self.marked_servers.clear();
                self.save()?;
                self.status = match self.language {
                    Language::English => format!("moved {count} server(s)"),
                    Language::ChineseSimplified => format!("已移动 {count} 个服务器"),
                };
                self.clear_input();
                self.mode = Mode::Normal;
                self.clamp_selection();
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
        let len = self.visible_tree().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len.saturating_sub(1) as isize) as usize;
        if self.main_view == MainView::Console {
            self.reset_console_for_selection();
        }
        self.detail_scroll = 0;
    }

    fn selected(&self) -> Option<&crate::config::ServerConfig> {
        self.selected_server_index()
            .and_then(|index| self.config.servers.get(index))
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected_server_index()
    }

    fn selected_mut(&mut self) -> Option<&mut crate::config::ServerConfig> {
        let index = self.selected_server_index()?;
        self.config.servers.get_mut(index)
    }

    fn selected_server_index(&self) -> Option<usize> {
        match self.visible_tree().get(self.selected).map(|row| &row.item) {
            Some(TreeItem::Server(index)) => Some(*index),
            _ => None,
        }
    }

    fn selected_group_id(&self) -> Option<String> {
        match self.visible_tree().get(self.selected).map(|row| &row.item) {
            Some(TreeItem::Group(id)) => Some(id.clone()),
            _ => None,
        }
    }

    fn visible_tree(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        self.push_tree_children(None, 0, &mut rows);
        rows
    }

    fn push_tree_children(&self, parent_id: Option<&str>, depth: usize, rows: &mut Vec<TreeRow>) {
        for group in self
            .config
            .groups
            .iter()
            .filter(|group| group.parent_id.as_deref() == parent_id)
        {
            rows.push(TreeRow {
                depth,
                item: TreeItem::Group(group.id.clone()),
            });
            if self.expanded_groups.contains(&group.id) {
                self.push_tree_children(Some(&group.id), depth + 1, rows);
            }
        }

        for (index, _server) in self
            .config
            .servers
            .iter()
            .enumerate()
            .filter(|(_, server)| {
                let group_exists = server.group_id.as_ref().is_some_and(|group_id| {
                    self.config.groups.iter().any(|group| &group.id == group_id)
                });
                server.group_id.as_deref() == parent_id || (parent_id.is_none() && !group_exists)
            })
        {
            rows.push(TreeRow {
                depth,
                item: TreeItem::Server(index),
            });
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_tree().len();
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(len - 1);
        }
    }

    fn delete_selected(&mut self) -> Result<()> {
        if let Some(index) = self.selected_server_index() {
            let removed = self.config.servers.remove(index);
            self.marked_servers.remove(&removed.id);
            self.clamp_selection();
            self.save()?;
            self.status = format!("{} {}", self.t(Text::Deleted), removed.name);
            return Ok(());
        }
        if let Some(group_id) = self.selected_group_id() {
            self.delete_group(&group_id)?;
        }
        Ok(())
    }

    fn delete_group(&mut self, group_id: &str) -> Result<()> {
        let Some(deleted) = self.config.delete_group_preserving_servers(group_id) else {
            return Ok(());
        };
        let removed_group_ids = deleted
            .removed_group_ids
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        self.expanded_groups
            .retain(|group_id| !removed_group_ids.contains(group_id));
        self.clamp_selection();
        self.save()?;
        self.status = match self.language {
            Language::English => format!(
                "deleted group {}; moved {} server(s)",
                deleted.name, deleted.moved_server_count
            ),
            Language::ChineseSimplified => format!(
                "已删除分组 {}；移动 {} 个服务器",
                deleted.name, deleted.moved_server_count
            ),
        };
        Ok(())
    }

    fn start_selected(&mut self) -> Result<()> {
        let Some(server) = self.selected().cloned() else {
            return Ok(());
        };
        process::start_supervisor(&self.store, &server)?;
        self.status = match self.language {
            Language::English => format!("started {}", server.name),
            Language::ChineseSimplified => format!("已启动 {}", server.name),
        };
        Ok(())
    }

    fn stop_selected(&mut self) -> Result<()> {
        let Some(server) = self.selected().cloned() else {
            return Ok(());
        };
        process::stop_server(&self.store, &server)?;
        self.status = match self.language {
            Language::English => format!("stopped {}", server.name),
            Language::ChineseSimplified => format!("已停止 {}", server.name),
        };
        Ok(())
    }

    fn restart_selected(&mut self) -> Result<()> {
        let Some(server) = self.selected().cloned() else {
            return Ok(());
        };
        process::stop_server(&self.store, &server)?;
        process::start_supervisor(&self.store, &server)?;
        self.status = match self.language {
            Language::English => format!("restarted {}", server.name),
            Language::ChineseSimplified => format!("已重启 {}", server.name),
        };
        Ok(())
    }

    fn restart_targets(&mut self) -> Result<()> {
        let servers = self.target_servers();
        for server in &servers {
            process::stop_server(&self.store, server)?;
        }
        for server in &servers {
            process::start_supervisor(&self.store, server)?;
        }
        self.status = match self.language {
            Language::English => format!("restarted {} server(s)", servers.len()),
            Language::ChineseSimplified => format!("已重启 {} 个服务器", servers.len()),
        };
        Ok(())
    }

    fn start_targets(&mut self) -> Result<()> {
        let servers = self.target_servers();
        for server in &servers {
            process::start_supervisor(&self.store, server)?;
        }
        self.status = match self.language {
            Language::English => format!("started {} server(s)", servers.len()),
            Language::ChineseSimplified => format!("已启动 {} 个服务器", servers.len()),
        };
        Ok(())
    }

    fn stop_targets(&mut self) -> Result<()> {
        let servers = self.target_servers();
        for server in &servers {
            process::stop_server(&self.store, server)?;
        }
        self.status = match self.language {
            Language::English => format!("stopped {} server(s)", servers.len()),
            Language::ChineseSimplified => format!("已停止 {} 个服务器", servers.len()),
        };
        Ok(())
    }

    fn target_servers(&self) -> Vec<crate::config::ServerConfig> {
        let ids = self.marked_or_selected_server_ids();
        self.config
            .servers
            .iter()
            .filter(|server| ids.contains(&server.id))
            .cloned()
            .collect()
    }

    fn marked_or_selected_server_ids(&self) -> Vec<String> {
        if !self.marked_servers.is_empty() {
            return self
                .config
                .servers
                .iter()
                .filter(|server| self.marked_servers.contains(&server.id))
                .map(|server| server.id.clone())
                .collect();
        }
        if let Some(server) = self.selected() {
            return vec![server.id.clone()];
        }
        if let Some(group_id) = self.selected_group_id() {
            return self.server_ids_in_group(&group_id);
        }
        Vec::new()
    }

    fn server_ids_in_group(&self, group_id: &str) -> Vec<String> {
        let mut ids = Vec::new();
        self.collect_server_ids_in_group(group_id, &mut ids);
        ids
    }

    fn collect_server_ids_in_group(&self, group_id: &str, ids: &mut Vec<String>) {
        for server in self
            .config
            .servers
            .iter()
            .filter(|server| server.group_id.as_deref() == Some(group_id))
        {
            ids.push(server.id.clone());
        }
        for child in self
            .config
            .groups
            .iter()
            .filter(|group| group.parent_id.as_deref() == Some(group_id))
        {
            self.collect_server_ids_in_group(&child.id, ids);
        }
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

    fn toggle_mark_selected(&mut self) {
        let Some(row) = self.visible_tree().get(self.selected).cloned() else {
            return;
        };
        match row.item {
            TreeItem::Server(index) => {
                if let Some(server) = self.config.servers.get(index) {
                    if !self.marked_servers.remove(&server.id) {
                        self.marked_servers.insert(server.id.clone());
                    }
                }
            }
            TreeItem::Group(group_id) => {
                let ids = self.server_ids_in_group(&group_id);
                let all_marked = ids.iter().all(|id| self.marked_servers.contains(id));
                if all_marked {
                    for id in ids {
                        self.marked_servers.remove(&id);
                    }
                } else {
                    self.marked_servers.extend(ids);
                }
            }
        }
    }

    fn expand_selected_group(&mut self) {
        if let Some(group_id) = self.selected_group_id() {
            self.expanded_groups.insert(group_id);
        }
    }

    fn collapse_selected_group(&mut self) {
        if let Some(group_id) = self.selected_group_id() {
            self.expanded_groups.remove(&group_id);
            self.clamp_selection();
        }
    }

    fn scroll_detail(&mut self, delta: isize) {
        if delta < 0 {
            self.detail_scroll = self
                .detail_scroll
                .saturating_sub(delta.unsigned_abs() as u16);
        } else {
            self.detail_scroll = self.detail_scroll.saturating_add(delta as u16);
        }
    }

    fn begin_add_group(&mut self) {
        self.clear_input();
        self.mode = Mode::AddGroup;
        self.status = match self.language {
            Language::English => "new group path, like proxy/velocity:".to_string(),
            Language::ChineseSimplified => "新分组路径，例如 proxy/velocity：".to_string(),
        };
    }

    fn begin_move_to_group(&mut self) {
        if self.marked_or_selected_server_ids().is_empty() {
            return;
        }
        self.clear_input();
        self.mode = Mode::MoveToGroup;
        self.status = match self.language {
            Language::English => "move to group path; blank moves to root:".to_string(),
            Language::ChineseSimplified => "移动到分组路径；留空移动到根目录：".to_string(),
        };
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

    fn send_console_terminal_input(&self, input: &str) -> Result<bool> {
        let Some(server) = self
            .selected()
            .filter(|server| {
                process::runtime_status(&self.store, server) == process::RuntimeStatus::Running
            })
            .cloned()
        else {
            return Ok(false);
        };
        process::append_terminal_input(&self.store, &server, input)?;
        Ok(true)
    }

    fn interrupt_console_server(&mut self) -> Result<()> {
        let Some(server) = self.selected().cloned() else {
            return Ok(());
        };
        if process::runtime_status(&self.store, &server) != process::RuntimeStatus::Running {
            return Ok(());
        }
        process::interrupt_server(&self.store, &server)?;
        self.console_follow = true;
        self.console_end = None;
        self.status = match self.language {
            Language::English => format!("sent Ctrl-C to {}", server.name),
            Language::ChineseSimplified => format!("已向 {} 发送 Ctrl-C", server.name),
        };
        Ok(())
    }

    fn queue_screen_clear(&mut self) {
        self.needs_screen_clear = true;
    }

    fn take_screen_clear(&mut self) -> bool {
        std::mem::take(&mut self.needs_screen_clear)
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
        self.queue_screen_clear();
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
        let visual_len = self.console_visual_len();
        if delta < 0 {
            let end = self.console_end.unwrap_or(visual_len);
            self.console_end = Some(end.saturating_sub(delta.unsigned_abs()).max(1));
            self.console_follow = false;
            self.status = self.t(Text::ConsolePaused).to_string();
        } else {
            let end = self.console_end.unwrap_or(visual_len);
            let next = end.saturating_add(delta as usize).min(visual_len);
            if next >= visual_len {
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

    fn set_console_wrap_width(&mut self, width: usize) {
        let width = width.max(1);
        if self.console_wrap_width != width {
            self.console_wrap_width = width;
            if !self.console_follow {
                let visual_len = self.console_visual_len();
                self.console_end = self.console_end.map(|end| end.min(visual_len).max(1));
            }
        }
    }

    fn console_visual_len(&self) -> usize {
        wrap_console_lines(&self.console_lines, self.console_wrap_width)
            .len()
            .max(self.console_lines.len().min(1))
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
            let len = self.console_visual_len();
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

    fn complete_prompt_input(&mut self) {
        let directory = match self.mode {
            Mode::Command | Mode::EditStartupCommand => {
                self.selected().map(|server| server.directory.clone())
            }
            Mode::AddStartupCommand => Some(PathBuf::from(self.draft.dir.trim())),
            _ => None,
        };
        if let Some(directory) = directory {
            complete_input_token(&mut self.input, &mut self.input_cursor, &directory);
        }
    }

    fn update_console_layout(&mut self, area: Rect) {
        if self.main_view != MainView::Console {
            return;
        }
        let header_height = if area.height >= 8 { 3 } else { 1 };
        let footer_height = if area.height >= 10 { 3 } else { 1 };
        let body_height = area
            .height
            .saturating_sub(header_height)
            .saturating_sub(footer_height);
        let console_header_height = if body_height < 6 {
            0
        } else if body_height >= 12 {
            5
        } else if body_height >= 8 {
            3
        } else {
            0
        };
        let console_height = body_height.saturating_sub(console_header_height);
        let input_height = if console_height >= 6 {
            3
        } else if console_height >= 3 {
            1
        } else {
            0
        };
        let log_height = console_height.saturating_sub(input_height);
        let bordered = log_height >= 3 && area.width >= 4;
        let width = if bordered {
            area.width.saturating_sub(2).max(1)
        } else {
            area.width.max(1)
        };
        self.set_console_wrap_width(width as usize);
    }

    fn t(&self, key: Text) -> &'static str {
        i18n::text(self.language, key)
    }

    fn save(&self) -> Result<()> {
        self.store.save(&self.config)
    }
}
