use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::config::{ConfigStore, RemiaftConfig, ServerConfig};
use crate::i18n::{self, Language, Text};
use crate::process;
use crate::shutdown;

mod console_log;
mod input;
mod render;
mod startup;
mod terminal;

use console_log::{ansi_to_line, wrap_console_lines};
use input::{
    backspace_at_cursor, complete_word, completion_display_candidates, delete_at_cursor, fallback,
    insert_at_cursor, move_cursor_left, move_cursor_right, Completion,
};
use startup::{apply_startup_command, normalize_startup_parts, parse_startup_command, split_args};
use terminal::TerminalGuard;

/// Keep at most this many raw log lines in memory for the embedded log panel.
const MAX_CONSOLE_LINES: usize = 2_000;
/// Only read the tail of the server log; the embedded panel never shows more.
const LOG_TAIL_BYTES: u64 = 512 * 1024;

pub async fn run(store: ConfigStore) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut app = App::new(store)?;

    loop {
        if shutdown::requested() {
            break;
        }
        app.drain_op_messages();
        app.refresh_console();
        let size = terminal.size()?;
        app.update_console_layout(size);
        app.ensure_wrapped();
        if app.take_screen_clear() {
            terminal.clear()?;
        }
        terminal.draw(|frame| render::draw(frame, &app))?;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) => {
                    if app.handle_key(key)? {
                        break;
                    }
                }
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
            }
        }
        if let Some(server) = app.take_console_attach_request() {
            terminal.suspend()?;
            let attach_result = process::attach_terminal(&app.store, &server);
            terminal.resume()?;
            app.queue_screen_clear();
            app.status = match attach_result {
                Ok(()) => app.t(Text::Help).to_string(),
                Err(err) => format!("console attach failed: {err}"),
            };
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

/// Lifecycle operation currently running for a server in a background
/// thread. Stopping can legitimately take up to 30 seconds (graceful stop,
/// then kill), so it must never run on the UI thread.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum PendingOp {
    Start,
    Stop,
    Restart,
}

/// State shared between the UI thread and lifecycle worker threads.
#[derive(Default)]
struct OpsShared {
    /// server id -> operation in flight.
    pending: Mutex<HashMap<String, PendingOp>>,
    /// Completion/error messages produced by worker threads, drained into
    /// the status line by the UI loop.
    messages: Mutex<Vec<String>>,
}

struct Draft {
    name: String,
    dir: String,
    startup_command: String,
}

/// Bash's double-Tab state, the equivalent of readline's
/// `rl_last_func == rl_complete && !completion_changed_buffer` check: a Tab
/// press that left the buffer unchanged arms listing, and the next
/// consecutive Tab displays the candidates instead of completing again.
/// Any other key disarms it.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum TabState {
    #[default]
    Fresh,
    /// The previous key was Tab and it did not modify the input.
    ArmedForList,
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
    show_details: bool,
    show_help: bool,
    console_server_id: Option<String>,
    console_lines: Vec<String>,
    console_wrapped: Vec<String>,
    console_dirty: bool,
    console_last_len: Option<u64>,
    console_end: Option<usize>,
    console_follow: bool,
    console_wrap_width: usize,
    console_attach_request: Option<ServerConfig>,
    needs_screen_clear: bool,
    tab_state: TabState,
    ops: Arc<OpsShared>,
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
            show_details: true,
            show_help: false,
            console_server_id: None,
            console_lines: Vec::new(),
            console_wrapped: Vec::new(),
            console_dirty: false,
            console_last_len: None,
            console_end: None,
            console_follow: true,
            console_wrap_width: 120,
            console_attach_request: None,
            needs_screen_clear: false,
            tab_state: TabState::default(),
            ops: Arc::new(OpsShared::default()),
        })
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.show_help {
            self.show_help = false;
            return Ok(false);
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }

        match self.mode {
            Mode::LanguageSelect => self.handle_language_key(key),
            Mode::Normal => self.handle_normal_key(key),
            _ => self.handle_input_key(key),
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !matches!(self.mode, Mode::Normal) {
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

    fn handle_normal_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char('?') | KeyCode::F(1) => self.show_help = true,
            KeyCode::F(2) => self.begin_add_group(),
            KeyCode::F(3) => self.begin_move_to_group(),
            KeyCode::F(5) => self.start_targets(),
            KeyCode::F(6) => self.stop_targets(),
            KeyCode::F(7) => self.restart_targets(),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Left => self.collapse_selected_group(),
            KeyCode::Right => self.expand_selected_group(),
            KeyCode::Enter => self.toggle_mark_selected(),
            KeyCode::PageUp => self.scroll_console(-10),
            KeyCode::PageDown => self.scroll_console(10),
            KeyCode::End => self.follow_console(),
            KeyCode::Char('n') => {
                self.mode = Mode::AddName;
                self.clear_input();
                self.status = self.t(Text::ServerNamePrompt).to_string();
            }
            KeyCode::Char('d') => self.delete_selected()?,
            KeyCode::Char('s') => self.start_selected(),
            KeyCode::Char('x') => self.stop_selected(),
            KeyCode::Char('r') => self.restart_selected(),
            KeyCode::Char('a') => self.toggle_auto_restart()?,
            KeyCode::Char('m') => self.begin_move_to_group(),
            KeyCode::Char('p') => self.begin_edit_dir(),
            KeyCode::Char('j') => self.begin_edit_jar(),
            KeyCode::Char('u') => self.begin_edit_startup_command(),
            KeyCode::Char('y') => self.begin_edit_java_path(),
            KeyCode::Char('e') => self.begin_edit_java_args(),
            KeyCode::Char('g') => self.begin_edit_server_args(),
            KeyCode::Char('c') | KeyCode::Char('i') => self.begin_command(),
            KeyCode::Char('o') => self.request_console_attach(),
            KeyCode::Char('b') => self.toggle_details(),
            KeyCode::Char('l') => {
                self.mode = Mode::LanguageSelect;
                self.status = i18n::text(self.language, Text::LanguagePromptHint).to_string();
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Result<bool> {
        if !matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.tab_state = TabState::Fresh;
        }
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
            KeyCode::Tab | KeyCode::BackTab => self.complete_prompt_input(),
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

    fn start_selected(&mut self) {
        if let Some(server) = self.selected().cloned() {
            self.queue_op(vec![server], PendingOp::Start);
        }
    }

    fn stop_selected(&mut self) {
        if let Some(server) = self.selected().cloned() {
            self.queue_op(vec![server], PendingOp::Stop);
        }
    }

    fn restart_selected(&mut self) {
        if let Some(server) = self.selected().cloned() {
            self.queue_op(vec![server], PendingOp::Restart);
        }
    }

    fn start_targets(&mut self) {
        let servers = self.target_servers();
        self.queue_op(servers, PendingOp::Start);
    }

    fn stop_targets(&mut self) {
        let servers = self.target_servers();
        self.queue_op(servers, PendingOp::Stop);
    }

    fn restart_targets(&mut self) {
        let servers = self.target_servers();
        self.queue_op(servers, PendingOp::Restart);
    }

    /// Runs a lifecycle operation on a worker thread. `stop_server` can block
    /// for up to 30 seconds waiting for a graceful shutdown, which used to
    /// freeze the whole UI; the pending map lets the tree show a transient
    /// "stopping..." state instead.
    fn queue_op(&mut self, servers: Vec<ServerConfig>, op: PendingOp) {
        let servers: Vec<ServerConfig> = {
            let pending = self.ops.pending.lock().expect("ops lock");
            servers
                .into_iter()
                .filter(|server| !pending.contains_key(&server.id))
                .collect()
        };
        if servers.is_empty() {
            return;
        }
        {
            let mut pending = self.ops.pending.lock().expect("ops lock");
            for server in &servers {
                pending.insert(server.id.clone(), op);
            }
        }

        self.status = op_progress_message(self.language, op, &servers);

        let ops = Arc::clone(&self.ops);
        let store = self.store.clone();
        let language = self.language;
        thread::spawn(move || {
            for server in servers {
                let result = match op {
                    PendingOp::Start => process::start_supervisor(&store, &server),
                    PendingOp::Stop => process::stop_server(&store, &server),
                    PendingOp::Restart => process::stop_server(&store, &server)
                        .and_then(|_| process::start_supervisor(&store, &server)),
                };
                ops.pending.lock().expect("ops lock").remove(&server.id);
                let message = match result {
                    Ok(()) => op_done_message(language, op, &server.name),
                    Err(err) => format!("{}: {err}", server.name),
                };
                ops.messages.lock().expect("ops lock").push(message);
            }
        });
    }

    fn drain_op_messages(&mut self) {
        let mut messages = self.ops.messages.lock().expect("ops lock");
        if let Some(last) = messages.pop() {
            self.status = last;
        }
        messages.clear();
    }

    fn pending_op(&self, server_id: &str) -> Option<PendingOp> {
        self.ops
            .pending
            .lock()
            .expect("ops lock")
            .get(server_id)
            .copied()
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

    fn queue_screen_clear(&mut self) {
        self.needs_screen_clear = true;
    }

    fn take_screen_clear(&mut self) -> bool {
        std::mem::take(&mut self.needs_screen_clear)
    }

    fn request_console_attach(&mut self) {
        let Some(server) = self.selected().cloned() else {
            return;
        };
        if process::runtime_status(&self.store, &server) != process::RuntimeStatus::Running {
            self.status = match self.language {
                Language::English => format!("{} is not running", server.name),
                Language::ChineseSimplified => format!("{} 未运行", server.name),
            };
            return;
        }
        self.console_attach_request = Some(server);
    }

    fn take_console_attach_request(&mut self) -> Option<ServerConfig> {
        self.console_attach_request.take()
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
        self.console_follow = true;
        self.console_end = None;
    }

    fn scroll_console(&mut self, delta: isize) {
        let visual_len = self.console_wrapped.len();
        if visual_len == 0 {
            return;
        }
        if delta < 0 {
            let end = self.console_end.unwrap_or(visual_len);
            self.console_end = Some(end.saturating_sub(delta.unsigned_abs()).max(1));
            self.console_follow = false;
        } else {
            let end = self.console_end.unwrap_or(visual_len);
            let next = end.saturating_add(delta as usize).min(visual_len);
            if next >= visual_len {
                self.console_follow = true;
                self.console_end = None;
            } else {
                self.console_end = Some(next.max(1));
            }
        }
    }

    fn reset_console_for_selection(&mut self) {
        self.console_server_id = self.selected().map(|server| server.id.clone());
        self.console_lines.clear();
        self.console_wrapped.clear();
        self.console_last_len = None;
        self.console_dirty = true;
        self.console_end = None;
        self.console_follow = true;
    }

    fn set_console_wrap_width(&mut self, width: usize) {
        let width = width.max(1);
        if self.console_wrap_width != width {
            self.console_wrap_width = width;
            self.console_dirty = true;
        }
    }

    /// Re-reads the tail of the selected server's log, but only when the
    /// file actually grew or the selection changed. Reading the whole file
    /// on every 200ms tick used to make the UI crawl once logs got large.
    fn refresh_console(&mut self) {
        let Some(server) = self.selected().cloned() else {
            if !self.console_lines.is_empty() {
                self.console_lines.clear();
                self.console_dirty = true;
            }
            self.console_server_id = None;
            self.console_last_len = None;
            return;
        };
        if self.console_server_id.as_deref() != Some(server.id.as_str()) {
            self.reset_console_for_selection();
        }
        let path = process::minecraft_log_path_for(&self.store, &server);
        let len = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if self.console_last_len == Some(len) {
            return;
        }
        self.console_last_len = Some(len);
        let content = read_log_tail(&path, LOG_TAIL_BYTES).unwrap_or_default();
        self.console_lines = content.lines().map(ToString::to_string).collect();
        if self.console_lines.len() > MAX_CONSOLE_LINES {
            let keep_from = self.console_lines.len() - MAX_CONSOLE_LINES;
            self.console_lines.drain(..keep_from);
        }
        self.console_dirty = true;
    }

    /// Rewraps the cached log lines when the content or panel width changed.
    /// Wrapping thousands of lines is too expensive to redo on every frame.
    fn ensure_wrapped(&mut self) {
        if !self.console_dirty {
            return;
        }
        self.console_wrapped = wrap_console_lines(&self.console_lines, self.console_wrap_width);
        self.console_dirty = false;
        if self.console_follow {
            self.console_end = None;
        } else {
            let len = self.console_wrapped.len();
            self.console_end = self.console_end.map(|end| end.min(len).max(1));
        }
    }

    fn console_visible_lines(&self, height: usize) -> Vec<Line<'static>> {
        if self.console_wrapped.is_empty() {
            return vec![Line::from(self.t(Text::ConsoleEmpty).to_string())];
        }

        let end = if self.console_follow {
            self.console_wrapped.len()
        } else {
            self.console_end
                .unwrap_or(self.console_wrapped.len())
                .min(self.console_wrapped.len())
                .max(1)
        };
        let start = end.saturating_sub(height);
        self.console_wrapped[start..end]
            .iter()
            .map(|line| ansi_to_line(line))
            .collect()
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
        self.tab_state = TabState::Fresh;
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.tab_state = TabState::Fresh;
    }

    /// The prompt modes where the first word is a command name (completed
    /// from PATH like bash's command position) rather than a plain filename.
    fn completion_command_position(&self) -> bool {
        matches!(
            self.mode,
            Mode::Command | Mode::EditStartupCommand | Mode::AddStartupCommand
        )
    }

    /// The directory the current prompt's relative filenames resolve
    /// against, i.e. bash's working directory for this "shell".
    fn completion_directory(&self) -> Option<PathBuf> {
        match self.mode {
            Mode::Command | Mode::EditStartupCommand | Mode::EditJar | Mode::EditServerArgs => {
                self.selected().map(|server| server.directory.clone())
            }
            Mode::AddStartupCommand => Some(PathBuf::from(self.draft.dir.trim())),
            Mode::AddDir | Mode::EditDir | Mode::EditJavaPath | Mode::EditJavaArgs => {
                Some(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
            }
            _ => None,
        }
    }

    /// readline's rl_complete: a Tab that modified the buffer completes; a
    /// second consecutive Tab that changed nothing lists the candidates.
    fn complete_prompt_input(&mut self) {
        let Some(directory) = self.completion_directory() else {
            return;
        };
        let command_position = self.completion_command_position();

        if self.tab_state == TabState::ArmedForList {
            let candidates = completion_display_candidates(
                &self.input,
                self.input_cursor,
                &directory,
                command_position,
            );
            if candidates.len() > 1 {
                self.status = candidate_status_message(self.language, &candidates);
            }
            return;
        }

        let before = (self.input.clone(), self.input_cursor);
        let completion = complete_word(
            &mut self.input,
            &mut self.input_cursor,
            &directory,
            command_position,
        );
        let changed = (self.input.as_str(), self.input_cursor) != (before.0.as_str(), before.1);
        // matching readline, both an unchanged partial completion and a
        // failed one arm the list; the next Tab shows what's ambiguous (or,
        // with no matches, silently finds nothing again)
        self.tab_state = if changed {
            TabState::Fresh
        } else {
            TabState::ArmedForList
        };
        if matches!(completion, Completion::Partial) && !changed {
            // bash rings the bell here; the status line is our bell
            self.status = self.t(Text::CompletionAmbiguousHint).to_string();
        }
    }

    fn update_console_layout(&mut self, area: Rect) {
        let regions = render::compute_regions(area, self.show_details);
        let log = regions.log;
        let bordered = log.height >= 3 && log.width >= 4;
        let width = if bordered {
            log.width.saturating_sub(2).max(1)
        } else {
            log.width.max(1)
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

/// Reads up to `tail_bytes` from the end of `path`, dropping a leading
/// partial line when the read did not start at the beginning of the file.
fn read_log_tail(path: &Path, tail_bytes: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let offset = len.saturating_sub(tail_bytes);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    // Drop the leading partial line before decoding: a tail read that starts
    // mid-character would otherwise make strict UTF-8 validation fail for the
    // whole buffer and misroute clean UTF-8 logs into the legacy fallback.
    let mut bytes = buf.as_slice();
    if offset > 0 {
        match bytes.iter().position(|byte| *byte == b'\n') {
            Some(newline) => bytes = &bytes[newline + 1..],
            None => bytes = &[],
        }
    }
    Some(crate::encoding::decode_console_bytes(bytes))
}

fn op_progress_message(language: Language, op: PendingOp, servers: &[ServerConfig]) -> String {
    let names = servers
        .iter()
        .map(|server| server.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    match (language, op) {
        (Language::English, PendingOp::Start) => format!("starting {names}"),
        (Language::English, PendingOp::Stop) => format!("stopping {names}"),
        (Language::English, PendingOp::Restart) => format!("restarting {names}"),
        (Language::ChineseSimplified, PendingOp::Start) => format!("正在启动 {names}"),
        (Language::ChineseSimplified, PendingOp::Stop) => format!("正在停止 {names}"),
        (Language::ChineseSimplified, PendingOp::Restart) => format!("正在重启 {names}"),
    }
}

fn op_done_message(language: Language, op: PendingOp, name: &str) -> String {
    match (language, op) {
        (Language::English, PendingOp::Start) => format!("started {name}"),
        (Language::English, PendingOp::Stop) => format!("stopped {name}"),
        (Language::English, PendingOp::Restart) => format!("restarted {name}"),
        (Language::ChineseSimplified, PendingOp::Start) => format!("已启动 {name}"),
        (Language::ChineseSimplified, PendingOp::Stop) => format!("已停止 {name}"),
        (Language::ChineseSimplified, PendingOp::Restart) => format!("已重启 {name}"),
    }
}

/// The second-Tab candidate listing (readline prints these below the
/// prompt in columns; here the status line is that display surface).
fn candidate_status_message(language: Language, candidates: &[String]) -> String {
    const MAX_SHOWN: usize = 12;
    let mut list = candidates
        .iter()
        .take(MAX_SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("  ");
    if candidates.len() > MAX_SHOWN {
        let more = candidates.len() - MAX_SHOWN;
        list.push_str(&match language {
            Language::English => format!("  (+{more} more)"),
            Language::ChineseSimplified => format!("  (还有 {more} 个)"),
        });
    }
    match language {
        Language::English => format!("{} matches: {list}", candidates.len()),
        Language::ChineseSimplified => format!("{} 个匹配项：{list}", candidates.len()),
    }
}
