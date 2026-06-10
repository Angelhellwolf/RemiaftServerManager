use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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

use crate::config::{ConfigStore, ServerConfig};

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
    match read_pid(&supervisor_pid_path(store, server)) {
        Ok(Some(pid)) if pid_alive(pid) => RuntimeStatus::Running,
        Ok(Some(_)) => RuntimeStatus::Stale,
        _ => RuntimeStatus::Stopped,
    }
}

pub fn start_supervisor(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    if runtime_status(store, server) == RuntimeStatus::Running {
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
    fs::create_dir_all(server_runtime_dir(store, server))?;
    fs::write(stop_flag_path(store, server), b"stop")?;
    append_terminal_input(store, server, "\u{3}")?;
    Ok(())
}

pub fn append_command(store: &ConfigStore, server: &ServerConfig, command: &str) -> Result<()> {
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
    let input = sanitize_terminal_input(input);
    if input.is_empty() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(command_path(store, server))?;
    file.write_all(input.as_bytes())?;
    file.flush()?;
    Ok(())
}

pub fn minecraft_log_path_for(store: &ConfigStore, server: &ServerConfig) -> PathBuf {
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
    let mut master_writer = master.try_clone()?;
    let command_thread = thread::spawn(move || {
        let mut offset = 0;
        while !command_done.load(Ordering::Relaxed) {
            if let Ok(new_offset) =
                pump_terminal_commands(&command_file, offset, &mut master_writer)
            {
                offset = new_offset;
            }
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

fn sanitize_terminal_input(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => {
                if let Some(sequence) = read_allowed_input_escape(&mut chars) {
                    output.push_str(sequence);
                }
            }
            '\u{8}' => output.push('\u{7f}'),
            '\r' | '\n' | '\t' | '\u{1}' | '\u{3}' | '\u{5}' | '\u{15}' | '\u{7f}' => {
                output.push(ch);
            }
            _ if is_allowed_console_char(ch) => output.push(ch),
            _ => {}
        }
    }

    output
}

pub fn is_allowed_console_char(ch: char) -> bool {
    ch >= ' ' && ch != '\u{7f}' && ch != '\u{a7}' && !ch.is_control()
}

fn read_allowed_input_escape<I>(chars: &mut std::iter::Peekable<I>) -> Option<&'static str>
where
    I: Iterator<Item = char>,
{
    let introducer = chars.next()?;
    match introducer {
        '[' => {
            let mut sequence = String::from("\u{1b}[");
            for ch in chars.by_ref() {
                sequence.push(ch);
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }

            match sequence.as_str() {
                "\u{1b}[A" => Some("\u{1b}[A"),
                "\u{1b}[B" => Some("\u{1b}[B"),
                "\u{1b}[C" => Some("\u{1b}[C"),
                "\u{1b}[D" => Some("\u{1b}[D"),
                "\u{1b}[F" => Some("\u{1b}[F"),
                "\u{1b}[H" => Some("\u{1b}[H"),
                "\u{1b}[3~" => Some("\u{1b}[3~"),
                _ => None,
            }
        }
        ']' => {
            while let Some(ch) = chars.next() {
                if ch == '\u{7}' {
                    break;
                }
                if ch == '\u{1b}' && chars.peek() == Some(&'\\') {
                    let _ = chars.next();
                    break;
                }
            }
            None
        }
        '(' | ')' => {
            let _ = chars.next();
            None
        }
        _ => None,
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
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    if !buf.is_empty() {
        let terminal_input = buf.replace('\n', "\r");
        terminal.write_all(terminal_input.as_bytes())?;
        terminal.flush()?;
    }
    Ok(offset + buf.len() as u64)
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
    if unsafe { libc::tcsetattr(slave_fd, libc::TCSANOW, &termios) } == -1 {
        return Err(std::io::Error::last_os_error()).context("set pty erase character");
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

    #[test]
    fn terminal_input_normalizes_backspace_to_delete() {
        assert_eq!(sanitize_terminal_input("sa\u{8}y\r"), "sa\u{7f}y\r");
    }

    #[test]
    fn terminal_input_keeps_known_editing_keys() {
        assert_eq!(
            sanitize_terminal_input(
                "\u{1b}[A\u{1b}[B\u{1b}[C\u{1b}[D\u{1b}[H\u{1b}[F\u{1b}[3~\u{1}\u{5}\u{15}\t"
            ),
            "\u{1b}[A\u{1b}[B\u{1b}[C\u{1b}[D\u{1b}[H\u{1b}[F\u{1b}[3~\u{1}\u{5}\u{15}\t"
        );
    }

    #[test]
    fn terminal_input_drops_mouse_and_osc_sequences() {
        assert_eq!(
            sanitize_terminal_input("say hi\u{1b}[<64;12;9M\u{1b}]0;title\u{7}\r"),
            "say hi\r"
        );
    }

    #[test]
    fn terminal_input_drops_disallowed_command_text() {
        assert_eq!(
            sanitize_terminal_input("say \u{a7}red\u{1b}bad\u{7f}\r"),
            "say redad\u{7f}\r"
        );
    }
}
