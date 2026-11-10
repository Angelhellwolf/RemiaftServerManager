use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::config::{ConfigStore, RemiaftConfig, ServerConfig};
use crate::encoding::StreamDecoder;
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
/// A small overlap verifies that an incremental read still follows the same
/// file contents after rotations or a fast truncate-and-regrow.
const LOG_TAIL_MARKER_BYTES: usize = 64;
/// Runtime state changes much less frequently than terminal input. Keeping a
/// short-lived snapshot avoids opening every PID file several times per frame.
const RUNTIME_STATUS_REFRESH_INTERVAL: Duration = Duration::from_millis(750);
/// Limit metadata polling for the selected log while keeping live output
/// responsive enough for an interactive dashboard.
const CONSOLE_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const CONFIG_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const UI_POLL_INTERVAL: Duration = Duration::from_millis(200);
const HELP_WINDOW_TIMEOUT: Duration = Duration::from_secs(10);

fn is_delete_shortcut(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(unix)]
fn has_terminal_hangup(revents: libc::c_short) -> bool {
    revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

#[cfg(unix)]
fn terminal_input_hung_up() -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(result > 0 && has_terminal_hangup(descriptor.revents))
}

#[cfg(not(unix))]
fn terminal_input_hung_up() -> std::io::Result<bool> {
    Ok(false)
}

fn window_expired(opened_at: Option<Instant>, now: Instant, timeout: Duration) -> bool {
    opened_at.is_some_and(|opened| now.saturating_duration_since(opened) >= timeout)
}

pub async fn run(store: ConfigStore) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let mut app = App::new(store)?;
    let mut needs_draw = true;
    let mut needs_layout = true;
    let mut last_size = None;
    // The language picker does not depend on server state. The normal screen
    // does: wait for its first asynchronous runtime snapshot so the terminal
    // receives one complete initial frame instead of an empty frame followed
    // immediately by a large differential update. Some SSH clients, notably
    // Xshell under load, can otherwise display only the second update.
    let mut initial_frame_ready = matches!(app.mode, Mode::LanguageSelect);

    loop {
        // A disconnected SSH/web terminal leaves its PTY descriptors open in
        // this process, but poll then reports HUP immediately. Without this
        // check the TUI loops at 100% CPU forever after its terminal vanishes.
        if shutdown::requested() || terminal_input_hung_up()? {
            break;
        }

        let operation_changed = app.drain_op_messages();
        needs_draw |= operation_changed;
        needs_draw |= app.refresh_external_config();
        needs_draw |= app.close_expired_windows();
        needs_draw |= app.refresh_pending_ops();
        needs_draw |= app.refresh_runtime_statuses(operation_changed);
        if !initial_frame_ready && app.runtime_status_snapshot_ready() {
            initial_frame_ready = true;
            needs_draw = true;
        }
        needs_draw |= app.refresh_console();
        if needs_layout {
            let size = terminal.size()?;
            if last_size != Some(size) {
                app.update_console_layout(size);
                last_size = Some(size);
                needs_draw = true;
            }
            needs_layout = false;
        }
        needs_draw |= app.ensure_wrapped();
        if app.take_screen_clear() {
            terminal.clear()?;
            needs_draw = true;
        }
        if needs_draw && initial_frame_ready {
            terminal.draw(|frame| render::draw(frame, &app))?;
            needs_draw = false;
        }
        let poll_interval = if initial_frame_ready {
            UI_POLL_INTERVAL
        } else {
            Duration::from_millis(20)
        };
        let event_ready = event::poll(poll_interval)?;
        // The PTY can disappear while poll is blocked. Check again before
        // entering crossterm's read path; optimized builds can otherwise
        // spin inside event::read forever on a hung-up descriptor.
        if terminal_input_hung_up()? {
            break;
        }
        if event_ready {
            match event::read()? {
                Event::Key(key) if !matches!(key.kind, KeyEventKind::Release) => {
                    if app.handle_key(key)? {
                        break;
                    }
                    needs_draw = true;
                }
                Event::Key(_) => {}
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse);
                    needs_draw = true;
                }
                Event::Resize(_, _) | Event::FocusGained => {
                    needs_layout = true;
                    needs_draw = true;
                }
                Event::FocusLost | Event::Paste(_) => {}
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
            app.invalidate_runtime_statuses();
            app.invalidate_console();
            needs_layout = true;
            needs_draw = true;
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
    DeleteConfirm,
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
    config_signature: Option<(u64, SystemTime)>,
    config_checked_at: Option<Instant>,
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
    help_opened_at: Option<Instant>,
    console_server_id: Option<String>,
    console_lines: Vec<String>,
    console_wrapped: Vec<String>,
    console_dirty: bool,
    console_decoder: StreamDecoder,
    console_decoder_preview: Option<String>,
    console_has_partial_line: bool,
    console_last_len: Option<u64>,
    console_last_modified: Option<SystemTime>,
    console_tail_marker: Vec<u8>,
    console_checked_at: Option<Instant>,
    console_end: Option<usize>,
    console_follow: bool,
    console_wrap_width: usize,
    runtime_statuses: HashMap<String, process::RuntimeStatus>,
    runtime_status_checked_at: Option<Instant>,
    runtime_status_scan: Option<mpsc::Receiver<HashMap<String, process::RuntimeStatus>>>,
    runtime_status_refresh_pending: bool,
    pending_ops: HashMap<String, PendingOp>,
    console_attach_request: Option<ServerConfig>,
    needs_screen_clear: bool,
    tab_state: TabState,
    ops: Arc<OpsShared>,
}

impl App {
    fn new(store: ConfigStore) -> Result<Self> {
        let config = store.load()?;
        let config_signature = store.signature();
        let expanded_groups = HashSet::new();
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
            config_signature,
            config_checked_at: None,
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
            help_opened_at: None,
            console_server_id: None,
            console_lines: Vec::new(),
            console_wrapped: Vec::new(),
            console_dirty: false,
            console_decoder: StreamDecoder::new(),
            console_decoder_preview: None,
            console_has_partial_line: false,
            console_last_len: None,
            console_last_modified: None,
            console_tail_marker: Vec::new(),
            console_checked_at: None,
            console_end: None,
            console_follow: true,
            console_wrap_width: 120,
            runtime_statuses: HashMap::new(),
            runtime_status_checked_at: None,
            runtime_status_scan: None,
            runtime_status_refresh_pending: false,
            pending_ops: HashMap::new(),
            console_attach_request: None,
            needs_screen_clear: false,
            tab_state: TabState::default(),
            ops: Arc::new(OpsShared::default()),
        })
    }

    fn refresh_external_config(&mut self) -> bool {
        if !matches!(self.mode, Mode::Normal)
            || self
                .config_checked_at
                .is_some_and(|checked| checked.elapsed() < CONFIG_REFRESH_INTERVAL)
        {
            return false;
        }
        self.config_checked_at = Some(Instant::now());
        let signature = self.store.signature();
        if signature == self.config_signature {
            return false;
        }

        let selected_server_id = self.selected().map(|server| server.id.clone());
        let selected_group_id = self.selected_group_id();
        let previous_server_ids = self
            .config
            .servers
            .iter()
            .map(|server| server.id.clone())
            .collect::<HashSet<_>>();
        let Ok(config) = self.store.load() else {
            return false;
        };
        self.config = config;
        self.config_signature = self.store.signature();

        let valid_group_ids = self
            .config
            .groups
            .iter()
            .map(|group| group.id.clone())
            .collect::<HashSet<_>>();
        self.expanded_groups
            .retain(|group_id| valid_group_ids.contains(group_id));
        let valid_server_ids = self
            .config
            .servers
            .iter()
            .map(|server| server.id.clone())
            .collect::<HashSet<_>>();
        self.marked_servers
            .retain(|server_id| valid_server_ids.contains(server_id));

        let new_group_ids = self
            .config
            .servers
            .iter()
            .filter(|server| !previous_server_ids.contains(&server.id))
            .filter_map(|server| server.group_id.clone())
            .collect::<Vec<_>>();
        for group_id in new_group_ids {
            self.expand_group_chain(&group_id);
        }

        let rows = self.visible_tree();
        self.selected = rows
            .iter()
            .position(|row| match &row.item {
                TreeItem::Server(index) => selected_server_id
                    .as_ref()
                    .is_some_and(|id| self.config.servers[*index].id == *id),
                TreeItem::Group(id) => selected_group_id.as_ref() == Some(id),
            })
            .unwrap_or_else(|| self.selected.min(rows.len().saturating_sub(1)));
        self.invalidate_runtime_statuses();
        self.invalidate_console();
        self.status = match self.language {
            Language::English => "server list updated".to_string(),
            Language::ChineseSimplified => "服务器列表已更新".to_string(),
        };
        true
    }

    fn expand_group_chain(&mut self, group_id: &str) {
        let mut current = Some(group_id.to_string());
        while let Some(id) = current {
            self.expanded_groups.insert(id.clone());
            current = self
                .config
                .groups
                .iter()
                .find(|group| group.id == id)
                .and_then(|group| group.parent_id.clone());
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.show_help {
            if matches!(key.kind, KeyEventKind::Repeat) {
                return Ok(false);
            }
            self.show_help = false;
            self.help_opened_at = None;
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
        if matches!(key.kind, KeyEventKind::Repeat)
            && !matches!(
                key.code,
                KeyCode::Down
                    | KeyCode::Up
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Char('k')
            )
        {
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.show_help = true;
                self.help_opened_at = Some(Instant::now());
            }
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
            KeyCode::Char('d') | KeyCode::Char('D') if is_delete_shortcut(&key) => {
                self.begin_delete_confirmation()
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.status = match self.language {
                    Language::English => "deletion requires Ctrl+D".to_string(),
                    Language::ChineseSimplified => "删除需同时按 Ctrl+D".to_string(),
                };
            }
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
        let console_server_after = self.selected().map(|server| server.id.clone());
        if self.console_server_id.as_deref() != console_server_after.as_deref() {
            self.invalidate_console();
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
            KeyCode::Enter => {
                self.commit_input()?;
                self.invalidate_console();
            }
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
                let ids = self
                    .marked_or_selected_server_ids()
                    .into_iter()
                    .collect::<HashSet<_>>();
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
            Mode::DeleteConfirm => {
                let expected = if let Some(server) = self.selected() {
                    Some(server.name.clone())
                } else if let Some(group_id) = self.selected_group_id() {
                    self.config
                        .groups
                        .iter()
                        .find(|group| group.id == group_id)
                        .map(|group| group.name.clone())
                } else {
                    None
                };
                let confirmed = expected
                    .as_deref()
                    .is_some_and(|name| self.input.trim() == name);
                self.clear_input();
                self.mode = Mode::Normal;
                if confirmed {
                    self.delete_selected()?;
                } else {
                    self.status = match self.language {
                        Language::English => {
                            "confirmation did not match; nothing deleted".to_string()
                        }
                        Language::ChineseSimplified => "名称不匹配，未删除任何内容".to_string(),
                    };
                }
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
        fn append_rows(
            parent_id: Option<&str>,
            depth: usize,
            groups_by_parent: &HashMap<Option<&str>, Vec<&crate::config::ServerGroup>>,
            servers_by_parent: &HashMap<Option<&str>, Vec<usize>>,
            expanded_groups: &HashSet<String>,
            rows: &mut Vec<TreeRow>,
        ) {
            if let Some(groups) = groups_by_parent.get(&parent_id) {
                for group in groups {
                    rows.push(TreeRow {
                        depth,
                        item: TreeItem::Group(group.id.clone()),
                    });
                    if expanded_groups.contains(&group.id) {
                        append_rows(
                            Some(group.id.as_str()),
                            depth + 1,
                            groups_by_parent,
                            servers_by_parent,
                            expanded_groups,
                            rows,
                        );
                    }
                }
            }
            if let Some(indices) = servers_by_parent.get(&parent_id) {
                rows.extend(indices.iter().map(|index| TreeRow {
                    depth,
                    item: TreeItem::Server(*index),
                }));
            }
        }

        let group_ids = self
            .config
            .groups
            .iter()
            .map(|group| group.id.as_str())
            .collect::<HashSet<_>>();
        let mut groups_by_parent: HashMap<Option<&str>, Vec<&crate::config::ServerGroup>> =
            HashMap::new();
        for group in &self.config.groups {
            groups_by_parent
                .entry(group.parent_id.as_deref())
                .or_default()
                .push(group);
        }
        let mut servers_by_parent: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
        for (index, server) in self.config.servers.iter().enumerate() {
            let parent_id = match server.group_id.as_deref() {
                Some(group_id) if group_ids.contains(group_id) => Some(group_id),
                _ => None,
            };
            servers_by_parent.entry(parent_id).or_default().push(index);
        }

        let mut rows = Vec::with_capacity(self.config.groups.len() + self.config.servers.len());
        append_rows(
            None,
            0,
            &groups_by_parent,
            &servers_by_parent,
            &self.expanded_groups,
            &mut rows,
        );
        rows
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

    fn begin_delete_confirmation(&mut self) {
        let target = if let Some(server) = self.selected() {
            Some(server.name.clone())
        } else if let Some(group_id) = self.selected_group_id() {
            self.config
                .groups
                .iter()
                .find(|group| group.id == group_id)
                .map(|group| group.name.clone())
        } else {
            None
        };
        let Some(target) = target else {
            return;
        };
        self.clear_input();
        self.mode = Mode::DeleteConfirm;
        self.status = match self.language {
            Language::English => {
                format!("type the exact name `{target}` and press Enter to delete")
            }
            Language::ChineseSimplified => {
                format!("二次确认：输入完整名称 `{target}` 后按 Enter 才会删除")
            }
        };
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

    fn drain_op_messages(&mut self) -> bool {
        let last = {
            let mut messages = self.ops.messages.lock().expect("ops lock");
            let last = messages.pop();
            messages.clear();
            last
        };
        if let Some(last) = last {
            self.status = last;
            return true;
        }
        false
    }

    /// Refreshes all server runtime states as one periodic snapshot. The scan
    /// runs off the UI thread: on a host with hundreds of configured servers,
    /// even one filesystem lookup per server is enough to make key handling
    /// visibly stall. Renderers only consult the last completed snapshot.
    fn refresh_runtime_statuses(&mut self, force: bool) -> bool {
        if force {
            // If a scan is already running, remember that an operation
            // completed during it and immediately follow it with a fresh one.
            self.runtime_status_refresh_pending = true;
            self.runtime_status_checked_at = None;
        }

        let mut changed = false;
        if let Some(receiver) = self.runtime_status_scan.as_ref() {
            match receiver.try_recv() {
                Ok(statuses) => {
                    self.runtime_status_scan = None;
                    self.runtime_status_checked_at = Some(Instant::now());
                    if statuses != self.runtime_statuses {
                        self.runtime_statuses = statuses;
                        changed = true;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.runtime_status_scan = None;
                    self.runtime_status_checked_at = Some(Instant::now());
                }
            }
        }

        let refresh_due = self
            .runtime_status_checked_at
            .is_none_or(|checked| checked.elapsed() >= RUNTIME_STATUS_REFRESH_INTERVAL);
        if self.runtime_status_scan.is_none()
            && (self.runtime_status_refresh_pending || refresh_due)
        {
            self.runtime_status_refresh_pending = false;
            let store = self.store.clone();
            let servers = self.config.servers.clone();
            let (sender, receiver) = mpsc::sync_channel(1);
            thread::spawn(move || {
                let statuses = servers
                    .iter()
                    .map(|server| (server.id.clone(), process::runtime_status(&store, server)))
                    .collect();
                let _ = sender.send(statuses);
            });
            self.runtime_status_scan = Some(receiver);
        }

        changed
    }

    fn invalidate_runtime_statuses(&mut self) {
        self.runtime_status_checked_at = None;
        self.runtime_status_refresh_pending = true;
    }

    fn runtime_status_snapshot_ready(&self) -> bool {
        self.runtime_status_checked_at.is_some()
    }

    fn runtime_status(&self, server: &ServerConfig) -> process::RuntimeStatus {
        self.runtime_statuses
            .get(&server.id)
            .copied()
            .unwrap_or(process::RuntimeStatus::Stopped)
    }

    fn refresh_pending_ops(&mut self) -> bool {
        let pending = self.ops.pending.lock().expect("ops lock").clone();
        if pending == self.pending_ops {
            return false;
        }
        self.pending_ops = pending;
        true
    }

    fn pending_op(&self, server_id: &str) -> Option<PendingOp> {
        self.pending_ops.get(server_id).copied()
    }

    fn target_servers(&self) -> Vec<crate::config::ServerConfig> {
        let ids = self
            .marked_or_selected_server_ids()
            .into_iter()
            .collect::<HashSet<_>>();
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

    /// Only transient, read-only overlays expire automatically. Input forms
    /// deliberately stay open so an idle timeout can never discard edits.
    fn close_expired_windows(&mut self) -> bool {
        if self.show_help
            && window_expired(self.help_opened_at, Instant::now(), HELP_WINDOW_TIMEOUT)
        {
            self.show_help = false;
            self.help_opened_at = None;
            return true;
        }
        false
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
        self.console_decoder = StreamDecoder::new();
        self.console_decoder_preview = None;
        self.console_has_partial_line = false;
        self.console_last_len = None;
        self.console_last_modified = None;
        self.console_tail_marker.clear();
        self.console_dirty = true;
        self.console_end = None;
        self.console_follow = true;
    }

    fn invalidate_console(&mut self) {
        self.console_checked_at = None;
    }

    fn set_console_wrap_width(&mut self, width: usize) {
        let width = width.max(1);
        if self.console_wrap_width != width {
            self.console_wrap_width = width;
            self.console_dirty = true;
        }
    }

    /// Incrementally reads newly appended bytes from the selected server log.
    /// A full tail reload is only needed for the initial view, truncation, or
    /// when more than the retained tail arrived between two refreshes.
    fn refresh_console(&mut self) -> bool {
        if self
            .console_checked_at
            .is_some_and(|checked| checked.elapsed() < CONSOLE_REFRESH_INTERVAL)
        {
            return false;
        }
        self.console_checked_at = Some(Instant::now());

        let Some(server) = self.selected().cloned() else {
            let changed = self.console_server_id.is_some() || !self.console_lines.is_empty();
            self.console_server_id = None;
            self.console_lines.clear();
            self.console_wrapped.clear();
            self.console_decoder = StreamDecoder::new();
            self.console_decoder_preview = None;
            self.console_has_partial_line = false;
            self.console_last_len = None;
            self.console_last_modified = None;
            self.console_tail_marker.clear();
            self.console_dirty |= changed;
            return changed;
        };
        let mut changed = false;
        if self.console_server_id.as_deref() != Some(server.id.as_str()) {
            self.reset_console_for_selection();
            changed = true;
        }

        let path = process::minecraft_log_path_for(&self.store, &server);
        let metadata = fs::metadata(&path).ok();
        let len = metadata.as_ref().map_or(0, fs::Metadata::len);
        let modified = metadata.and_then(|metadata| metadata.modified().ok());
        let Some(previous_len) = self.console_last_len else {
            return self.reload_console_tail(&path) || changed;
        };
        if log_needs_reload(previous_len, self.console_last_modified, len, modified) {
            return self.reload_console_tail(&path) || changed;
        }
        if len == previous_len {
            return changed;
        }
        if len.saturating_sub(previous_len) > LOG_TAIL_BYTES {
            return self.reload_console_tail(&path) || changed;
        }

        let Some((read, prefix_matches)) = read_log_from(
            &path,
            previous_len,
            LOG_TAIL_BYTES,
            &self.console_tail_marker,
        ) else {
            return changed;
        };
        if !prefix_matches {
            return self.reload_console_tail(&path) || changed;
        }
        let LogRead {
            end,
            bytes,
            tail_marker,
            modified,
        } = read;
        self.console_last_len = Some(end);
        self.console_last_modified = modified;
        self.console_tail_marker = tail_marker;
        changed |= self.append_console_bytes(&bytes);
        self.trim_console_history(true);
        self.console_dirty |= changed;
        changed
    }

    fn reload_console_tail(&mut self, path: &Path) -> bool {
        let had_content = !self.console_lines.is_empty();
        self.console_lines.clear();
        self.console_wrapped.clear();
        self.console_decoder = StreamDecoder::new();
        self.console_decoder_preview = None;
        self.console_has_partial_line = false;

        let LogRead {
            end,
            bytes,
            tail_marker,
            modified,
        } = read_log_tail_bytes(path, LOG_TAIL_BYTES).unwrap_or_default();
        self.console_last_len = Some(end);
        self.console_last_modified = modified;
        self.console_tail_marker = tail_marker;
        let appended = self.append_console_bytes(&bytes);
        self.trim_console_history(false);
        let changed = had_content || appended;
        self.console_dirty = true;
        changed
    }

    fn trim_console_history(&mut self, preserve_scroll_anchor: bool) {
        if self.console_lines.len() <= MAX_CONSOLE_LINES {
            return;
        }
        let keep_from = self.console_lines.len() - MAX_CONSOLE_LINES;
        if preserve_scroll_anchor && !self.console_follow {
            let removed_visual_lines =
                wrap_console_lines(&self.console_lines[..keep_from], self.console_wrap_width).len();
            self.console_end = self
                .console_end
                .map(|end| end.saturating_sub(removed_visual_lines).max(1));
        }
        self.console_lines.drain(..keep_from);
    }

    fn append_console_bytes(&mut self, bytes: &[u8]) -> bool {
        let mut changed = self.remove_console_decoder_preview();
        let text = self.console_decoder.push(bytes);
        changed |= append_console_text(
            &mut self.console_lines,
            &mut self.console_has_partial_line,
            &text,
        );

        let preview = self.console_decoder.pending_preview();
        if !preview.is_empty() {
            changed |= append_console_text(
                &mut self.console_lines,
                &mut self.console_has_partial_line,
                &preview,
            );
            self.console_decoder_preview = Some(preview);
        }
        changed
    }

    fn remove_console_decoder_preview(&mut self) -> bool {
        let Some(preview) = self.console_decoder_preview.take() else {
            return false;
        };
        let Some(line) = self.console_lines.last_mut() else {
            return false;
        };
        if !line.ends_with(&preview) {
            return false;
        }
        line.truncate(line.len() - preview.len());
        true
    }

    /// Rewraps the cached log lines when the content or panel width changed.
    /// Wrapping thousands of lines is too expensive to redo on every frame.
    fn ensure_wrapped(&mut self) -> bool {
        if !self.console_dirty {
            return false;
        }
        self.console_wrapped = wrap_console_lines(&self.console_lines, self.console_wrap_width);
        self.console_dirty = false;
        if self.console_follow {
            self.console_end = None;
        } else {
            let len = self.console_wrapped.len();
            self.console_end = self.console_end.map(|end| end.min(len).max(1));
        }
        true
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

    fn save(&mut self) -> Result<()> {
        self.store.save(&self.config)?;
        self.config_signature = self.store.signature();
        self.config_checked_at = Some(Instant::now());
        Ok(())
    }
}

#[derive(Default)]
struct LogRead {
    end: u64,
    bytes: Vec<u8>,
    tail_marker: Vec<u8>,
    modified: Option<SystemTime>,
}

fn log_needs_reload(
    previous_len: u64,
    previous_modified: Option<SystemTime>,
    len: u64,
    modified: Option<SystemTime>,
) -> bool {
    len < previous_len || (len == previous_len && modified != previous_modified)
}

fn log_tail_marker(bytes: &[u8]) -> Vec<u8> {
    bytes[bytes.len().saturating_sub(LOG_TAIL_MARKER_BYTES)..].to_vec()
}

/// Reads up to `tail_bytes` from the end of `path`, dropping a leading
/// partial line when the read did not start at the beginning of the file.
/// The returned offset always points to the end of all bytes read, including
/// a dropped prefix, so the next incremental read never repeats data.
fn read_log_tail_bytes(path: &Path, tail_bytes: u64) -> Option<LogRead> {
    let mut file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let len = metadata.len();
    let modified = metadata.modified().ok();
    let offset = len.saturating_sub(tail_bytes);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    file.take(len.saturating_sub(offset))
        .read_to_end(&mut buf)
        .ok()?;
    let end = offset.saturating_add(buf.len() as u64);
    let tail_marker = log_tail_marker(&buf);
    // Drop the leading partial line before decoding: a tail read that starts
    // mid-character would otherwise make strict UTF-8 validation fail for the
    // whole buffer and misroute clean UTF-8 logs into the legacy fallback.
    if offset > 0 {
        let keep_from = buf
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buf.len(), |newline| newline + 1);
        buf.drain(..keep_from);
    }
    Some(LogRead {
        end,
        bytes: buf,
        tail_marker,
        modified,
    })
}

fn read_log_from(
    path: &Path,
    offset: u64,
    max_bytes: u64,
    expected_tail: &[u8],
) -> Option<(LogRead, bool)> {
    let mut file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let len = metadata.len();
    let modified = metadata.modified().ok();
    let read_len = len.checked_sub(offset)?;
    if read_len > max_bytes {
        return None;
    }
    let start = offset.saturating_sub(expected_tail.len() as u64);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(len.saturating_sub(start))
        .read_to_end(&mut buf)
        .ok()?;
    let end = start.saturating_add(buf.len() as u64);
    let prefix_matches = buf.starts_with(expected_tail);
    let tail_marker = log_tail_marker(&buf);
    let bytes = if prefix_matches {
        buf[expected_tail.len()..].to_vec()
    } else {
        Vec::new()
    };
    Some((
        LogRead {
            end,
            bytes,
            tail_marker,
            modified,
        },
        prefix_matches,
    ))
}

/// Appends decoded stream text while preserving an unfinished final line so
/// that a later chunk can extend it instead of creating a duplicate row.
fn append_console_text(lines: &mut Vec<String>, has_partial_line: &mut bool, text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    let mut combined = String::new();
    if *has_partial_line {
        combined = lines.pop().unwrap_or_default();
    }
    combined.push_str(text);

    let ends_with_newline = combined.ends_with('\n');
    let mut parts = combined.split('\n').peekable();
    while let Some(part) = parts.next() {
        let is_final = parts.peek().is_none();
        if is_final && ends_with_newline {
            break;
        }
        let line = if is_final {
            part
        } else {
            part.strip_suffix('\r').unwrap_or(part)
        };
        lines.push(line.to_string());
    }
    *has_partial_line = !ends_with_newline;
    true
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

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{
        append_console_text, is_delete_shortcut, log_needs_reload, read_log_from,
        read_log_tail_bytes, window_expired, HELP_WINDOW_TIMEOUT, LOG_TAIL_BYTES,
    };

    #[cfg(unix)]
    use super::has_terminal_hangup;

    #[test]
    fn deletion_requires_control_and_d_together() {
        assert!(!is_delete_shortcut(&KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::NONE,
        )));
        assert!(!is_delete_shortcut(&KeyEvent::new(
            KeyCode::Char('D'),
            KeyModifiers::SHIFT,
        )));
        assert!(is_delete_shortcut(&KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_hangup_flags_are_detected_without_treating_input_as_closed() {
        assert!(has_terminal_hangup(libc::POLLHUP));
        assert!(has_terminal_hangup(libc::POLLERR));
        assert!(has_terminal_hangup(libc::POLLNVAL));
        assert!(!has_terminal_hangup(libc::POLLIN));
    }

    #[test]
    fn transient_window_expires_at_its_deadline() {
        let opened = Instant::now();

        assert!(!window_expired(
            None,
            opened + HELP_WINDOW_TIMEOUT,
            HELP_WINDOW_TIMEOUT,
        ));
        assert!(!window_expired(
            Some(opened),
            opened + Duration::from_secs(9),
            HELP_WINDOW_TIMEOUT,
        ));
        assert!(window_expired(
            Some(opened),
            opened + HELP_WINDOW_TIMEOUT,
            HELP_WINDOW_TIMEOUT,
        ));
    }

    #[test]
    fn console_text_extends_the_previous_partial_line() {
        let mut lines = Vec::new();
        let mut partial = false;

        assert!(append_console_text(
            &mut lines,
            &mut partial,
            "first\npartial",
        ));
        assert_eq!(lines, vec!["first".to_string(), "partial".to_string()]);
        assert!(partial);

        assert!(append_console_text(&mut lines, &mut partial, " line\nnext",));
        assert_eq!(
            lines,
            vec![
                "first".to_string(),
                "partial line".to_string(),
                "next".to_string(),
            ]
        );
        assert!(partial);

        assert!(append_console_text(&mut lines, &mut partial, "\n"));
        assert_eq!(
            lines,
            vec![
                "first".to_string(),
                "partial line".to_string(),
                "next".to_string(),
            ]
        );
        assert!(!partial);
    }

    #[test]
    fn console_text_preserves_blank_lines_and_normalizes_crlf() {
        let mut lines = Vec::new();
        let mut partial = false;

        append_console_text(&mut lines, &mut partial, "one\r\n\nthree\r");

        assert_eq!(
            lines,
            vec!["one".to_string(), "".to_string(), "three\r".to_string()]
        );
        assert!(partial);
    }

    #[test]
    fn console_text_preserves_standalone_carriage_return_redraws() {
        let mut lines = Vec::new();
        let mut partial = false;

        append_console_text(&mut lines, &mut partial, "10%\r");
        append_console_text(&mut lines, &mut partial, "20%\r");

        assert_eq!(lines, vec!["10%\r20%\r".to_string()]);
        assert!(partial);
    }

    #[test]
    fn same_length_log_rewrite_requires_a_reload() {
        let first = UNIX_EPOCH + Duration::from_secs(1);
        let second = first + Duration::from_secs(1);

        assert!(!log_needs_reload(128, Some(first), 128, Some(first)));
        assert!(log_needs_reload(128, Some(first), 128, Some(second)));
        assert!(log_needs_reload(128, Some(first), 64, Some(second)));
        assert!(!log_needs_reload(128, Some(first), 256, Some(second)));
    }

    #[test]
    fn incremental_log_read_detects_truncate_and_regrow() {
        let path =
            std::env::temp_dir().join(format!("remiaft-log-cursor-{}", uuid::Uuid::new_v4()));
        fs::write(&path, b"old line\nold tail\n").unwrap();
        let initial = read_log_tail_bytes(&path, LOG_TAIL_BYTES).unwrap();

        let mut writer = OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"appended\n").unwrap();
        writer.flush().unwrap();
        let (appended, prefix_matches) =
            read_log_from(&path, initial.end, LOG_TAIL_BYTES, &initial.tail_marker).unwrap();
        assert!(prefix_matches);
        assert_eq!(appended.bytes, b"appended\n");

        drop(writer);
        let replacement_len = appended.end as usize + 32;
        fs::write(&path, vec![b'x'; replacement_len]).unwrap();
        let (_, prefix_matches) =
            read_log_from(&path, appended.end, LOG_TAIL_BYTES, &appended.tail_marker).unwrap();
        assert!(!prefix_matches);

        fs::remove_file(path).unwrap();
    }
}
