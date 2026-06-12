use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::ptr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::cursor;
use crossterm::execute;
use crossterm::terminal::{self, disable_raw_mode, enable_raw_mode, ClearType};

use crate::config::{ConfigStore, RemiaftConfig, ServerConfig, ServerRuntimeKind};
use crate::docker;
use crate::text_encoding;

const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(50);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RuntimeStatus {
    Running,
    Stopped,
    Stale,
}

impl RuntimeStatus {
    pub fn label(self) -> &'static str {
        match self {
            RuntimeStatus::Running => "running",
            RuntimeStatus::Stopped => "stopped",
            RuntimeStatus::Stale => "stale",
        }
    }
}

pub fn runtime_status(store: &ConfigStore, server: &ServerConfig) -> RuntimeStatus {
    if server.uses_docker() {
        return match docker::runtime_status(server) {
            docker::RuntimeStatus::Running => RuntimeStatus::Running,
            docker::RuntimeStatus::Stopped => RuntimeStatus::Stopped,
            docker::RuntimeStatus::Stale => RuntimeStatus::Stale,
        };
    }
    native_runtime_status(store, server)
}

fn native_runtime_status(store: &ConfigStore, server: &ServerConfig) -> RuntimeStatus {
    match read_pid(&supervisor_pid_path(store, server)) {
        Ok(Some(pid)) if pid_alive(pid) => RuntimeStatus::Running,
        Ok(Some(_)) => RuntimeStatus::Stale,
        _ => RuntimeStatus::Stopped,
    }
}

pub fn start_server(store: &ConfigStore, config: &mut RemiaftConfig, key: &str) -> Result<()> {
    let index = config.find_server_index(key)?;
    if config.servers[index].runtime.kind == ServerRuntimeKind::Docker {
        let reserved_ports = reserved_docker_ports(config, &config.servers[index].id);
        docker::prepare_server(&mut config.servers[index], &reserved_ports)?;
        store.save(config)?;
        return docker::start_server(store, &config.servers[index]);
    }
    start_supervisor(store, &config.servers[index])
}

pub fn restart_server(store: &ConfigStore, config: &mut RemiaftConfig, key: &str) -> Result<()> {
    let server = config.find_server(key)?.clone();
    stop_server(store, &server)?;
    start_server(store, config, key)
}

pub fn start_supervisor(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    if native_runtime_status(store, server) == RuntimeStatus::Running {
        return Ok(());
    }

    fs::create_dir_all(server_runtime_dir(store, server))?;
    let exe = std::env::current_exe().context("resolve current executable")?;
    let log = supervisor_log_path(store, server);
    let stdout = append_file(&log)?;
    let stderr = stdout.try_clone()?;

    let mut command = Command::new(exe);
    command
        .arg("supervise")
        .arg(&server.id)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    detach_from_terminal(&mut command);
    command.spawn().context("spawn remiaft supervisor")?;

    Ok(())
}

pub fn stop_server(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    if server.uses_docker() {
        return docker::stop_server(store, server);
    }
    stop_native_server(store, server)
}

fn stop_native_server(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    fs::create_dir_all(server_runtime_dir(store, server))?;
    fs::write(stop_flag_path(store, server), b"stop")?;
    append_command(store, server, "stop")?;

    let start = Instant::now();
    while start.elapsed() < STOP_TIMEOUT {
        if runtime_status(store, server) != RuntimeStatus::Running {
            return Ok(());
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }

    if let Some(pid) = read_pid(&child_pid_path(store, server))? {
        kill_pid(pid)?;
    }
    if let Some(pid) = read_pid(&supervisor_pid_path(store, server))? {
        kill_pid(pid)?;
    }
    Ok(())
}

pub fn interrupt_server(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    if server.uses_docker() {
        return docker::interrupt_server(store, server);
    }
    fs::create_dir_all(server_runtime_dir(store, server))?;
    fs::write(stop_flag_path(store, server), b"stop")?;
    append_terminal_input(store, server, "\u{3}")?;
    Ok(())
}

pub fn append_command(store: &ConfigStore, server: &ServerConfig, command: &str) -> Result<()> {
    if server.uses_docker() {
        docker::send_rcon_command(server, command)?;
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(command_path(store, server))?;
    writeln!(file, "{command}")?;
    Ok(())
}

pub fn append_terminal_input(
    store: &ConfigStore,
    server: &ServerConfig,
    input: &str,
) -> Result<()> {
    if server.uses_docker() {
        let command = input.trim_matches(['\r', '\n']);
        if !command.trim().is_empty() {
            docker::send_rcon_command(server, command.trim())?;
        }
        return Ok(());
    }
    append_terminal_bytes_to_queue(store, server, input.as_bytes())
}

pub fn request_terminal_resize(
    store: &ConfigStore,
    server: &ServerConfig,
    rows: u16,
    cols: u16,
) -> Result<()> {
    if server.uses_docker() {
        return Ok(());
    }
    fs::create_dir_all(server_runtime_dir(store, server))?;
    fs::write(resize_path(store, server), format!("{rows} {cols}\n"))?;
    Ok(())
}

pub fn attach_terminal(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    if server.uses_docker() {
        return docker::attach_rcon_console(store, server);
    }
    if runtime_status(store, server) != RuntimeStatus::Running {
        return Err(anyhow!("{} is not running", server.name));
    }
    fs::create_dir_all(server_runtime_dir(store, server))?;
    let _raw = AttachTerminalGuard::enter()?;
    let (cols, rows) = terminal::size().unwrap_or((120, 40));
    let _ = request_terminal_resize(store, server, rows, cols);

    let log_path = minecraft_log_path(store, server);
    let log_len = fs::metadata(&log_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let replay_offset = log_len.saturating_sub(64 * 1024);
    let done = Arc::new(AtomicBool::new(false));

    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    write!(
        stdout,
        "\r\n-- remiaft native console: {} --\r\nCtrl-U detach | Ctrl-C interrupt server | Up/Down command history\r\n\r\n",
        server.name
    )?;
    replay_terminal_output(&minecraft_log_path(store, server), replay_offset)?;
    stdout.flush()?;

    let output_done = Arc::clone(&done);
    let output_thread = thread::spawn(move || tail_terminal_output(log_path, log_len, output_done));

    let mut stdin = io::stdin().lock();
    let mut buf = [0_u8; 1024];
    let mut input_state = AttachInputState::default();
    loop {
        let read = stdin.read(&mut buf)?;
        if read == 0 {
            break;
        }
        let (cols, rows) = terminal::size().unwrap_or((cols, rows));
        let _ = request_terminal_resize(store, server, rows, cols);

        for action in input_state.process(&buf[..read]) {
            match action {
                AttachInputAction::Forward(bytes) => {
                    append_terminal_bytes(store, server, &bytes)?;
                }
                AttachInputAction::Detach => {
                    done.store(true, Ordering::Relaxed);
                    let _ = output_thread.join();
                    stdout.write_all(b"\r\n-- detached --\r\n")?;
                    stdout.flush()?;
                    return Ok(());
                }
                AttachInputAction::Interrupt => {
                    interrupt_server(store, server)?;
                }
            }
        }
    }

    done.store(true, Ordering::Relaxed);
    let _ = output_thread.join();
    Ok(())
}

#[derive(Default)]
struct AttachInputState {
    current_line: Vec<u8>,
    history: Vec<Vec<u8>>,
    history_index: Option<usize>,
    pending_escape: Vec<u8>,
}

enum AttachInputAction {
    Forward(Vec<u8>),
    Detach,
    Interrupt,
}

enum AttachEscape {
    Up,
    Down,
    Other,
}

enum EscapeParse {
    Complete { len: usize, key: AttachEscape },
    Incomplete,
}

impl AttachInputState {
    fn process(&mut self, input: &[u8]) -> Vec<AttachInputAction> {
        let mut data = Vec::new();
        if !self.pending_escape.is_empty() {
            data.extend_from_slice(&self.pending_escape);
            self.pending_escape.clear();
        }
        data.extend_from_slice(input);

        let mut actions = Vec::new();
        let mut index = 0;
        while index < data.len() {
            if data[index] == 0xc2 && data.get(index + 1) == Some(&0xa7) {
                index += 2;
                continue;
            }

            match data[index] {
                0x1b => match parse_attach_escape(&data[index..]) {
                    EscapeParse::Complete { len, key } => {
                        if let Some(bytes) = self.handle_escape(key) {
                            actions.push(AttachInputAction::Forward(bytes));
                        }
                        index += len;
                    }
                    EscapeParse::Incomplete => {
                        self.pending_escape.extend_from_slice(&data[index..]);
                        break;
                    }
                },
                0x15 => {
                    actions.push(AttachInputAction::Detach);
                    index += 1;
                }
                0x03 => {
                    self.current_line.clear();
                    self.history_index = None;
                    actions.push(AttachInputAction::Interrupt);
                    index += 1;
                }
                b'\r' | b'\n' => {
                    self.commit_current_line();
                    actions.push(AttachInputAction::Forward(vec![data[index]]));
                    index += 1;
                }
                0x08 | 0x7f => {
                    self.current_line.pop();
                    self.history_index = None;
                    actions.push(AttachInputAction::Forward(vec![data[index]]));
                    index += 1;
                }
                b'\t' => {
                    self.history_index = None;
                    actions.push(AttachInputAction::Forward(vec![b'\t']));
                    index += 1;
                }
                0x20..=0x7e | 0x80..=0xff => {
                    self.current_line.push(data[index]);
                    self.history_index = None;
                    actions.push(AttachInputAction::Forward(vec![data[index]]));
                    index += 1;
                }
                0x04 => {
                    actions.push(AttachInputAction::Forward(vec![data[index]]));
                    index += 1;
                }
                _ => {
                    index += 1;
                }
            }
        }

        actions
    }

    fn handle_escape(&mut self, key: AttachEscape) -> Option<Vec<u8>> {
        match key {
            AttachEscape::Up => self.recall_history(-1),
            AttachEscape::Down => self.recall_history(1),
            AttachEscape::Other => None,
        }
    }

    fn recall_history(&mut self, direction: isize) -> Option<Vec<u8>> {
        if self.history.is_empty() {
            return None;
        }

        let next_index = if direction < 0 {
            Some(
                self.history_index
                    .map(|index| index.saturating_sub(1))
                    .unwrap_or_else(|| self.history.len() - 1),
            )
        } else {
            match self.history_index {
                Some(index) if index + 1 < self.history.len() => Some(index + 1),
                Some(_) => None,
                None => return None,
            }
        };

        let next_line = next_index
            .and_then(|index| self.history.get(index).cloned())
            .unwrap_or_default();
        let mut replacement = vec![0x7f; self.current_line.len()];
        replacement.extend_from_slice(&next_line);
        self.current_line = next_line;
        self.history_index = next_index;
        Some(replacement)
    }

    fn commit_current_line(&mut self) {
        let line = std::mem::take(&mut self.current_line);
        self.history_index = None;
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            return;
        }
        if self.history.last().is_some_and(|last| last == &line) {
            return;
        }
        self.history.push(line);
        if self.history.len() > 100 {
            self.history.remove(0);
        }
    }
}

fn parse_attach_escape(input: &[u8]) -> EscapeParse {
    match input {
        [0x1b] | [0x1b, b'['] | [0x1b, b'O'] => EscapeParse::Incomplete,
        [0x1b, b'[', rest @ ..] => {
            let Some(end) = rest.iter().position(|byte| (b'@'..=b'~').contains(byte)) else {
                return EscapeParse::Incomplete;
            };
            let final_char = rest[end];
            let key = match final_char {
                b'A' => AttachEscape::Up,
                b'B' => AttachEscape::Down,
                _ => AttachEscape::Other,
            };
            EscapeParse::Complete { len: end + 3, key }
        }
        [0x1b, b'O', ch, ..] => {
            let key = match ch {
                b'A' => AttachEscape::Up,
                b'B' => AttachEscape::Down,
                _ => AttachEscape::Other,
            };
            EscapeParse::Complete { len: 3, key }
        }
        [0x1b, ..] => EscapeParse::Complete {
            len: skip_escape_bytes(input).max(1),
            key: AttachEscape::Other,
        },
        _ => EscapeParse::Complete {
            len: 0,
            key: AttachEscape::Other,
        },
    }
}

fn append_terminal_bytes(store: &ConfigStore, server: &ServerConfig, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    append_terminal_bytes_to_queue(store, server, bytes)
}

fn append_terminal_bytes_to_queue(
    store: &ConfigStore,
    server: &ServerConfig,
    bytes: &[u8],
) -> Result<()> {
    let input = sanitize_terminal_bytes(bytes);
    if input.is_empty() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(command_path(store, server))?;
    file.write_all(&input)?;
    file.flush()?;
    Ok(())
}

fn tail_terminal_output(path: PathBuf, mut offset: u64, done: Arc<AtomicBool>) {
    let mut stdout = io::stdout();
    let mut buf = [0_u8; 8192];
    while !done.load(Ordering::Relaxed) {
        if let Ok(mut file) = File::open(&path) {
            if let Ok(len) = file.metadata().map(|metadata| metadata.len()) {
                if len < offset {
                    offset = 0;
                }
            }
            if file.seek(SeekFrom::Start(offset)).is_ok() {
                loop {
                    match file.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            offset = offset.saturating_add(n as u64);
                            let text = text_encoding::decode_console_bytes(&buf[..n]);
                            let _ = stdout.write_all(text.as_bytes());
                            let _ = stdout.flush();
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn replay_terminal_output(path: &Path, offset: u64) -> Result<()> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut stdout = io::stdout();
    let mut buf = [0_u8; 8192];
    loop {
        match file.read(&mut buf)? {
            0 => break,
            n => {
                let text = text_encoding::decode_console_bytes(&buf[..n]);
                stdout.write_all(text.as_bytes())?;
            }
        }
    }
    stdout.flush()?;
    Ok(())
}

struct AttachTerminalGuard;

impl AttachTerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, cursor::Show)?;
        Ok(Self)
    }
}

impl Drop for AttachTerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), cursor::Show);
    }
}

pub fn minecraft_log_path_for(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
    if server.uses_docker() {
        let _ = docker::sync_logs(store, server);
        return docker::minecraft_log_path(store, server);
    }
    minecraft_log_path(store, server)
}

pub fn run_supervisor(store: &ConfigStore, server_id: &str) -> Result<()> {
    let config = store.load()?;
    let server = config.find_server(server_id)?.clone();
    fs::create_dir_all(server_runtime_dir(store, &server))?;
    fs::write(
        supervisor_pid_path(store, &server),
        std::process::id().to_string(),
    )?;
    let _ = fs::remove_file(stop_flag_path(store, &server));

    loop {
        let exit_code = run_server_once(store, &config.java_path, &server)?;
        if stop_flag_path(store, &server).exists() || !server.auto_restart {
            cleanup_runtime(store, &server);
            return Ok(());
        }
        append_supervisor_log(
            store,
            &server,
            &format!(
                "server exited with code {:?}; restarting in {}s\n",
                exit_code, server.restart_delay_secs
            ),
        )?;
        thread::sleep(Duration::from_secs(server.restart_delay_secs));
    }
}

#[cfg(unix)]
fn run_server_once(
    store: &ConfigStore,
    default_java: &str,
    server: &ServerConfig,
) -> Result<Option<i32>> {
    if !server.directory.exists() {
        return Err(anyhow!(
            "server directory does not exist: {}",
            server.directory.display()
        ));
    }

    fs::write(command_path(store, server), b"")?;

    let (master, slave_fd) = open_pty()?;
    configure_pty_slave(slave_fd)?;
    set_nonblocking(&master)?;
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let stdin = slave.try_clone()?;
    let stdout = slave.try_clone()?;
    let stderr = slave.try_clone()?;

    let mut command = launch_command(default_java, server);
    command
        .current_dir(&server.directory)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("CLICOLOR_FORCE", "1")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::tcsetpgrp(slave_fd, libc::getpgrp()) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn server process {}", server.name))?;
    drop(slave);

    fs::write(child_pid_path(store, server), child.id().to_string())?;
    let done = Arc::new(AtomicBool::new(false));
    let command_done = Arc::clone(&done);
    let command_file = command_path(store, server);
    let resize_file = resize_path(store, server);
    let mut master_writer = master.try_clone()?;
    let command_thread = thread::spawn(move || {
        let mut offset = 0;
        let mut size = None;
        while !command_done.load(Ordering::Relaxed) {
            if let Ok(new_offset) =
                pump_terminal_commands(&command_file, offset, &mut master_writer)
            {
                offset = new_offset;
            }
            let _ = pump_terminal_resize(&resize_file, &master_writer, &mut size);
            thread::sleep(COMMAND_POLL_INTERVAL);
        }
    });

    let output_done = Arc::clone(&done);
    let mut master_reader = master;
    let mut log = append_file(&minecraft_log_path(store, server))?;
    let output_thread = thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match master_reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = log.write_all(&buf[..n]);
                    let _ = log.flush();
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if output_done.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });

    let status = child.wait().context("wait Minecraft server")?;
    done.store(true, Ordering::Relaxed);
    let _ = command_thread.join();
    let _ = output_thread.join();
    let _ = fs::remove_file(child_pid_path(store, server));
    Ok(status.code())
}

#[cfg(not(unix))]
fn run_server_once(
    store: &ConfigStore,
    default_java: &str,
    server: &ServerConfig,
) -> Result<Option<i32>> {
    if !server.directory.exists() {
        return Err(anyhow!(
            "server directory does not exist: {}",
            server.directory.display()
        ));
    }

    fs::write(command_path(store, server), b"")?;
    let log = append_file(&minecraft_log_path(store, server))?;
    let stderr = log.try_clone()?;

    let mut command = launch_command(default_java, server);
    command
        .current_dir(&server.directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn server process {}", server.name))?;

    fs::write(child_pid_path(store, server), child.id().to_string())?;
    let done = Arc::new(AtomicBool::new(false));
    let command_done = Arc::clone(&done);
    let command_file = command_path(store, server);
    let mut stdin = child.stdin.take().context("open child stdin")?;
    let command_thread = thread::spawn(move || {
        let mut offset = 0;
        while !command_done.load(Ordering::Relaxed) {
            if let Ok(new_offset) = pump_commands(&command_file, offset, &mut stdin) {
                offset = new_offset;
            }
            thread::sleep(COMMAND_POLL_INTERVAL);
        }
    });

    let status = child.wait().context("wait Minecraft server")?;
    done.store(true, Ordering::Relaxed);
    let _ = command_thread.join();
    let _ = fs::remove_file(child_pid_path(store, server));
    Ok(status.code())
}

fn java_args(server: &ServerConfig) -> Vec<String> {
    let mut args = vec![
        format!("-Xms{}M", server.min_memory_mb),
        format!("-Xmx{}M", server.max_memory_mb),
    ];
    args.extend(server.java_args.clone());
    args.push("-jar".to_string());
    args.push(server.jar_path.to_string_lossy().to_string());
    args.extend(server.server_args.clone());
    args
}

fn launch_command(default_java: &str, server: &ServerConfig) -> Command {
    if let Some(startup_command) = server
        .startup_command
        .as_deref()
        .map(str::trim)
        .filter(|command| !command.is_empty())
    {
        shell_command(startup_command)
    } else {
        let mut command = Command::new(server.java_bin(default_java));
        command.args(java_args(server));
        command
    }
}

fn sanitize_terminal_bytes(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;

    while index < input.len() {
        if input[index] == 0xc2 && input.get(index + 1) == Some(&0xa7) {
            index += 2;
            continue;
        }
        match input[index] {
            0x1b => {
                index += skip_escape_bytes(&input[index..]).max(1);
            }
            0x08 => {
                output.push(0x7f);
                index += 1;
            }
            b'\r' | b'\n' | b'\t' | 0x03 | 0x04 | 0x7f => {
                output.push(input[index]);
                index += 1;
            }
            0x20..=0x7e => {
                output.push(input[index]);
                index += 1;
            }
            0x80..=0xff => {
                output.push(input[index]);
                index += 1;
            }
            _ => {
                index += 1;
            }
        }
    }

    output
}

pub fn is_allowed_console_char(ch: char) -> bool {
    ch >= ' ' && ch != '\u{7f}' && ch != '\u{a7}' && !ch.is_control()
}

fn skip_escape_bytes(input: &[u8]) -> usize {
    match input {
        [0x1b, b'[', rest @ ..] => rest
            .iter()
            .position(|byte| (b'@'..=b'~').contains(byte))
            .map(|end| 2 + end + 1)
            .unwrap_or(input.len()),
        [0x1b, b']', rest @ ..] => {
            let mut index = 2;
            while index < 2 + rest.len() {
                if input[index] == 0x07 {
                    return index + 1;
                }
                if input[index] == 0x1b && input.get(index + 1) == Some(&b'\\') {
                    return index + 2;
                }
                index += 1;
            }
            input.len()
        }
        [0x1b, b'(' | b')', ..] => input.len().min(3),
        [0x1b, ..] => input.len().min(2),
        _ => 0,
    }
}

#[cfg(unix)]
fn shell_command(command_line: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(command_line);
    command
}

#[cfg(windows)]
fn shell_command(command_line: &str) -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C").arg(command_line);
    command
}

#[cfg(not(unix))]
fn pump_commands(path: &Path, offset: u64, stdin: &mut impl Write) -> Result<u64> {
    if !path.exists() {
        return Ok(offset);
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    if !buf.is_empty() {
        stdin.write_all(buf.as_bytes())?;
        stdin.flush()?;
    }
    Ok(offset + buf.len() as u64)
}

#[cfg(unix)]
fn pump_terminal_commands(path: &Path, offset: u64, terminal: &mut impl Write) -> Result<u64> {
    if !path.exists() {
        return Ok(offset);
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let read_len = buf.len();
    if !buf.is_empty() {
        let terminal_input = buf
            .into_iter()
            .map(|byte| if byte == b'\n' { b'\r' } else { byte })
            .collect::<Vec<_>>();
        terminal.write_all(&terminal_input)?;
        terminal.flush()?;
    }
    Ok(offset + read_len as u64)
}

#[cfg(unix)]
fn pump_terminal_resize(
    path: &Path,
    terminal: &File,
    last_size: &mut Option<(u16, u16)>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(path)?;
    let mut parts = raw.split_whitespace();
    let Some(rows) = parts.next().and_then(|value| value.parse::<u16>().ok()) else {
        return Ok(());
    };
    let Some(cols) = parts.next().and_then(|value| value.parse::<u16>().ok()) else {
        return Ok(());
    };
    let size = (rows.max(1), cols.max(1));
    if *last_size == Some(size) {
        return Ok(());
    }
    set_pty_size(terminal.as_raw_fd(), size.0, size.1)?;
    *last_size = Some(size);
    Ok(())
}

#[cfg(unix)]
fn open_pty() -> Result<(File, i32)> {
    let mut master_fd = 0;
    let mut slave_fd = 0;
    #[cfg(target_vendor = "apple")]
    let rc = {
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: 160,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut size,
            )
        }
    };
    #[cfg(not(target_vendor = "apple"))]
    let rc = {
        let size = libc::winsize {
            ws_row: 40,
            ws_col: 160,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                ptr::null_mut(),
                ptr::null(),
                &size,
            )
        }
    };
    if rc == -1 {
        return Err(std::io::Error::last_os_error()).context("open pty");
    }
    Ok((unsafe { File::from_raw_fd(master_fd) }, slave_fd))
}

#[cfg(unix)]
fn configure_pty_slave(slave_fd: i32) -> Result<()> {
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(slave_fd, &mut termios) } == -1 {
        return Err(std::io::Error::last_os_error()).context("read pty termios");
    }
    termios.c_cc[libc::VERASE] = 0x7f;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        termios.c_oflag &= !libc::TABDLY;
    }
    #[cfg(target_vendor = "apple")]
    {
        termios.c_oflag &= !libc::OXTABS;
    }
    if unsafe { libc::tcsetattr(slave_fd, libc::TCSANOW, &termios) } == -1 {
        return Err(std::io::Error::last_os_error()).context("configure pty terminal flags");
    }
    Ok(())
}

#[cfg(unix)]
fn set_pty_size(fd: i32, rows: u16, cols: u16) -> Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as libc::c_ulong, &size) } == -1 {
        return Err(std::io::Error::last_os_error()).context("resize pty");
    }
    Ok(())
}

#[cfg(unix)]
fn set_nonblocking(file: &File) -> Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error()).context("read pty flags");
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error()).context("set pty nonblocking");
    }
    Ok(())
}

fn cleanup_runtime(store: &ConfigStore, server: &ServerConfig) {
    let _ = fs::remove_file(supervisor_pid_path(store, server));
    let _ = fs::remove_file(child_pid_path(store, server));
    let _ = fs::remove_file(stop_flag_path(store, server));
    let _ = fs::remove_file(resize_path(store, server));
}

fn append_supervisor_log(store: &ConfigStore, server: &ServerConfig, line: &str) -> Result<()> {
    let mut file = append_file(&supervisor_log_path(store, server))?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

fn append_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn reserved_docker_ports(config: &RemiaftConfig, except_server_id: &str) -> Vec<u16> {
    config
        .servers
        .iter()
        .filter(|server| server.id != except_server_id)
        .flat_map(|server| {
            let docker = &server.runtime.docker;
            docker
                .rcon
                .host_port
                .into_iter()
                .chain(docker.ports.iter().filter_map(|port| port.host_port))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(unix)]
fn detach_from_terminal(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_from_terminal(_command: &mut Command) {}

fn read_pid(path: &Path) -> Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    Ok(raw.trim().parse::<u32>().ok())
}

fn server_runtime_dir(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
    store.runtime_dir().join(&server.id)
}

fn supervisor_pid_path(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
    server_runtime_dir(store, server).join("supervisor.pid")
}

fn child_pid_path(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
    server_runtime_dir(store, server).join("server.pid")
}

fn command_path(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
    server_runtime_dir(store, server).join("commands.in")
}

fn resize_path(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
    server_runtime_dir(store, server).join("terminal.size")
}

fn stop_flag_path(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
    server_runtime_dir(store, server).join("stop.flag")
}

fn supervisor_log_path(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
    server_runtime_dir(store, server).join("supervisor.log")
}

fn minecraft_log_path(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
    server_runtime_dir(store, server).join("minecraft.log")
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    Command::new("cmd")
        .args([
            "/C",
            &format!("tasklist /FI \"PID eq {pid}\" | findstr {pid}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn kill_pid(pid: u32) -> Result<()> {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}

#[cfg(windows)]
fn kill_pid(pid: u32) -> Result<()> {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sanitize_text(input: &str) -> String {
        String::from_utf8_lossy(&sanitize_terminal_bytes(input.as_bytes())).into_owned()
    }

    fn forward_text(actions: Vec<AttachInputAction>) -> String {
        let bytes = actions
            .into_iter()
            .filter_map(|action| match action {
                AttachInputAction::Forward(bytes) => Some(bytes),
                AttachInputAction::Detach | AttachInputAction::Interrupt => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn terminal_input_normalizes_backspace_to_delete() {
        assert_eq!(sanitize_text("sa\u{8}y\r"), "sa\u{7f}y\r");
    }

    #[test]
    fn terminal_input_drops_escape_editing_keys_but_keeps_tab() {
        assert_eq!(
            sanitize_text(
                "\u{1b}[A\u{1b}[B\u{1b}[C\u{1b}[D\u{1b}[H\u{1b}[F\u{1b}[3~\u{1}\u{5}\u{15}\t"
            ),
            "\t"
        );
    }

    #[test]
    fn terminal_input_drops_mouse_and_osc_sequences() {
        assert_eq!(
            sanitize_text("say hi\u{1b}[<64;12;9M\u{1b}]0;title\u{7}\r"),
            "say hi\r"
        );
    }

    #[test]
    fn terminal_input_drops_disallowed_command_text() {
        assert_eq!(
            sanitize_text("say \u{a7}red\u{1b}bad\u{7f}\r"),
            "say redad\u{7f}\r"
        );
    }

    #[test]
    fn attach_input_up_and_down_recall_command_history() {
        let mut state = AttachInputState::default();

        assert_eq!(forward_text(state.process(b"say one\r")), "say one\r");
        assert_eq!(forward_text(state.process(b"say two\r")), "say two\r");
        assert_eq!(forward_text(state.process(b"\x1b[A")), "say two");
        assert_eq!(
            forward_text(state.process(b"\x1b[A")),
            "\u{7f}\u{7f}\u{7f}\u{7f}\u{7f}\u{7f}\u{7f}say one"
        );
        assert_eq!(
            forward_text(state.process(b"\x1b[B")),
            "\u{7f}\u{7f}\u{7f}\u{7f}\u{7f}\u{7f}\u{7f}say two"
        );
        assert_eq!(
            forward_text(state.process(b"\x1b[B")),
            "\u{7f}\u{7f}\u{7f}\u{7f}\u{7f}\u{7f}\u{7f}"
        );
    }
}
