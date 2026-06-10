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
    while start.elapsed() < Duration::from_secs(30) {
        if runtime_status(store, server) != RuntimeStatus::Running {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }

    if let Some(pid) = read_pid(&child_pid_path(store, server))? {
        kill_pid(pid)?;
    }
    if let Some(pid) = read_pid(&supervisor_pid_path(store, server))? {
        kill_pid(pid)?;
    }
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
    set_nonblocking(&master)?;
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let stdin = slave.try_clone()?;
    let stdout = slave.try_clone()?;
    let stderr = slave.try_clone()?;

    let mut command = Command::new(server.java_bin(default_java));
    command
        .current_dir(&server.directory)
        .args(java_args(server))
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
        .with_context(|| format!("spawn Minecraft server {}", server.name))?;
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
            thread::sleep(Duration::from_millis(300));
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

    let mut child = Command::new(server.java_bin(default_java))
        .current_dir(&server.directory)
        .args(java_args(server))
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| format!("spawn Minecraft server {}", server.name))?;

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
            thread::sleep(Duration::from_millis(300));
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
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 160,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut size,
        )
    };
    if rc == -1 {
        return Err(std::io::Error::last_os_error()).context("open pty");
    }
    Ok((unsafe { File::from_raw_fd(master_fd) }, slave_fd))
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
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn kill_pid(pid: u32) -> Result<()> {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()?;
    Ok(())
}

#[cfg(windows)]
fn kill_pid(pid: u32) -> Result<()> {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()?;
    Ok(())
}
