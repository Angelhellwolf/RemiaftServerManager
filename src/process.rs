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

use crate::config::{ConfigStore, ServerConfig};
use crate::encoding::StreamDecoder;
use crate::shutdown;

const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(unix)]
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SUPERVISOR_READY_TIMEOUT: Duration = Duration::from_secs(3);
const SUPERVISOR_READY_POLL_INTERVAL: Duration = Duration::from_millis(25);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(500);
const ATTACH_POLL_INTERVAL: Duration = Duration::from_millis(200);
const CONSOLE_EXIT_RETURN_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACH_LOG_REOPEN_INTERVAL: Duration = Duration::from_secs(1);
const ATTACH_LOG_MARKER_BYTES: usize = 64;
const ATTACH_REPLAY_BYTES: u64 = 64 * 1024;

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
    // Clear only a stop request left by an earlier run. Once the child has
    // been spawned, a new stop flag belongs to this launch and must never be
    // erased by a slow supervisor startup.
    let _ = fs::remove_file(stop_flag_path(store, server));
    // Publish the current terminal size before the supervisor spawns so its
    // PTY is born at the width it will actually be viewed at. The server's
    // console (JLine) records width-dependent cursor sequences into the log;
    // if the PTY starts at some default width and is only resized once a
    // client attaches, the entire startup burst is formatted for the wrong
    // width and replays as scrambled, double-column output.
    if let Ok((cols, rows)) = terminal::size() {
        let _ = request_terminal_resize(store, server, rows, cols);
    }
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
    let mut child = command.spawn().context("spawn remiaft supervisor")?;
    let ready_started = Instant::now();
    let mut ready = false;
    while ready_started.elapsed() < SUPERVISOR_READY_TIMEOUT {
        if runtime_status(store, server) == RuntimeStatus::Running {
            ready = true;
            break;
        }
        let child_status = match child.try_wait() {
            Ok(status) => status,
            Err(err) => {
                thread::spawn(move || {
                    let _ = child.wait();
                });
                return Err(err).context("check remiaft supervisor");
            }
        };
        if let Some(status) = child_status {
            return Err(anyhow!("remiaft supervisor exited early with {status}"));
        }
        thread::sleep(SUPERVISOR_READY_POLL_INTERVAL);
    }
    if !ready {
        ready = runtime_status(store, server) == RuntimeStatus::Running;
    }
    if !ready {
        let child_status = match child.try_wait() {
            Ok(status) => status,
            Err(err) => {
                thread::spawn(move || {
                    let _ = child.wait();
                });
                return Err(err).context("check remiaft supervisor");
            }
        };
        if let Some(status) = child_status {
            return Err(anyhow!("remiaft supervisor exited early with {status}"));
        }
    }
    // The supervisor detaches into its own session, but it is still a
    // direct OS child of this process; nobody else will ever reap it, so
    // wait on it from a throwaway thread instead of leaking a zombie once it
    // exits.
    thread::spawn(move || {
        let _ = child.wait();
    });

    // A still-live supervisor owns the launch request even if an unusually
    // slow filesystem delayed PID publication past the handshake window.
    // Returning an error here would encourage a retry and risk two
    // supervisors; the periodic runtime snapshot will observe late readiness.
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
    append_terminal_bytes_to_queue(store, server, input.as_bytes())
}

pub fn request_terminal_resize(
    store: &ConfigStore,
    server: &ServerConfig,
    rows: u16,
    cols: u16,
) -> Result<()> {
    fs::create_dir_all(server_runtime_dir(store, server))?;
    fs::write(resize_path(store, server), format!("{rows} {cols}\n"))?;
    Ok(())
}

pub fn attach_terminal(store: &ConfigStore, server: &ServerConfig) -> Result<()> {
    if runtime_status(store, server) != RuntimeStatus::Running {
        return Err(anyhow!("{} is not running", server.name));
    }
    fs::create_dir_all(server_runtime_dir(store, server))?;
    let mut terminal_guard = AttachTerminalGuard::enter()?;
    let (cols, rows) = terminal::size().unwrap_or((120, 40));
    let mut published_size = request_terminal_resize(store, server, rows, cols)
        .is_ok()
        .then_some((cols, rows));

    let log_path = minecraft_log_path(store, server);
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    write!(
        stdout,
        "\r\n-- remiaft native console: {} --\r\nCtrl-U detach | Ctrl-C interrupt server | terminal scrollback for history\r\n\r\n",
        server.name
    )?;
    let (replay_end, replay_marker) = replay_terminal_output(&log_path)?;
    stdout.flush()?;

    terminal_guard.start_output(log_path, replay_end, replay_marker);

    let mut stdin = io::stdin().lock();
    let mut buf = [0_u8; 1024];
    let (mut cols, mut rows) = (cols, rows);
    loop {
        if shutdown::requested() {
            break;
        }

        let Some(read) = read_attach_input(&mut stdin, &mut buf, ATTACH_POLL_INTERVAL)? else {
            if runtime_status(store, server) != RuntimeStatus::Running {
                terminal_guard.stop_output();
                write!(
                    stdout,
                    "\r\n-- {} exited --\r\n-- press any key to return (auto-return in {}s) --\r\n",
                    server.name,
                    CONSOLE_EXIT_RETURN_TIMEOUT.as_secs()
                )?;
                stdout.flush()?;
                // Release the long-lived stdin lock before the platform
                // specific key poller reads from the terminal.
                drop(stdin);
                wait_for_any_key(CONSOLE_EXIT_RETURN_TIMEOUT)?;
                return Ok(());
            }
            let (new_cols, new_rows) = terminal::size().unwrap_or((cols, rows));
            (cols, rows) = (new_cols, new_rows);
            if published_size != Some((cols, rows))
                && request_terminal_resize(store, server, rows, cols).is_ok()
            {
                published_size = Some((cols, rows));
            }
            continue;
        };

        if read == 0 {
            break;
        }
        let (new_cols, new_rows) = terminal::size().unwrap_or((cols, rows));
        (cols, rows) = (new_cols, new_rows);
        if published_size != Some((cols, rows))
            && request_terminal_resize(store, server, rows, cols).is_ok()
        {
            published_size = Some((cols, rows));
        }

        let mut chunk_start = 0;
        for index in 0..read {
            match buf[index] {
                0x15 => {
                    append_terminal_bytes(store, server, &buf[chunk_start..index])?;
                    terminal_guard.stop_output();
                    stdout.write_all(b"\r\n-- detached --\r\n")?;
                    stdout.flush()?;
                    return Ok(());
                }
                0x03 => {
                    append_terminal_bytes(store, server, &buf[chunk_start..index])?;
                    interrupt_server(store, server)?;
                    chunk_start = index + 1;
                }
                _ => {}
            }
        }
        append_terminal_bytes(store, server, &buf[chunk_start..read])?;
    }

    Ok(())
}

/// Blocks until stdin has data ready to read or `timeout` elapses, returning
/// whether data is ready.
#[cfg(unix)]
fn wait_stdin_ready(timeout: Duration) -> Result<bool> {
    let mut fds = [libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    }];
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
    if rc < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error).context("poll stdin");
    }
    Ok(rc > 0)
}

#[cfg(unix)]
fn read_attach_input(
    stdin: &mut impl Read,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<Option<usize>> {
    if !wait_stdin_ready(timeout)? {
        return Ok(None);
    }
    Ok(Some(stdin.read(buf)?))
}

#[cfg(windows)]
fn read_attach_input(
    _stdin: &mut impl Read,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<Option<usize>> {
    use crossterm::event::{self, Event, KeyEventKind};

    if !event::poll(timeout)? {
        return Ok(None);
    }
    let Event::Key(key) = event::read()? else {
        return Ok(None);
    };
    if matches!(key.kind, KeyEventKind::Release) {
        return Ok(None);
    }
    let input = windows_key_bytes(key);
    let read = input.len().min(buf.len());
    buf[..read].copy_from_slice(&input[..read]);
    Ok((read > 0).then_some(read))
}

#[cfg(not(any(unix, windows)))]
fn read_attach_input(
    stdin: &mut impl Read,
    buf: &mut [u8],
    _timeout: Duration,
) -> Result<Option<usize>> {
    Ok(Some(stdin.read(buf)?))
}

#[cfg(any(windows, test))]
fn windows_key_bytes(key: crossterm::event::KeyEvent) -> Vec<u8> {
    use crossterm::event::{KeyCode, KeyModifiers};

    // Windows reports AltGr text as Ctrl+Alt plus the already composed
    // character. Treat that combination as text, otherwise characters such
    // as `@` and `\u{20ac}` would be rewritten as terminal control sequences.
    let alt_gr_text = matches!(key.code, KeyCode::Char(_))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.contains(KeyModifiers::ALT);
    let mut bytes = Vec::new();
    if key.modifiers.contains(KeyModifiers::ALT) && !alt_gr_text {
        bytes.push(0x1b);
    }

    match key.code {
        KeyCode::Char(ch) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) && !alt_gr_text {
                let normalized = ch.to_ascii_lowercase();
                let control = match normalized {
                    '@' | ' ' | '2' => Some(0x00),
                    'a'..='z' => Some(normalized as u8 - b'a' + 1),
                    '[' | '3' => Some(0x1b),
                    '\\' | '4' => Some(0x1c),
                    ']' | '5' => Some(0x1d),
                    '^' | '6' => Some(0x1e),
                    '_' | '7' => Some(0x1f),
                    '?' | '8' => Some(0x7f),
                    _ => None,
                };
                if let Some(control) = control {
                    bytes.push(control);
                    return bytes;
                }
            }
            let mut encoded = [0_u8; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
        }
        KeyCode::Backspace => bytes.push(0x7f),
        KeyCode::Enter => bytes.push(b'\r'),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::BackTab => bytes.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => bytes.push(0x1b),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
        KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
        KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
        KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
        KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
        KeyCode::F(number) => {
            let sequence: &[u8] = match number {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => b"",
            };
            bytes.extend_from_slice(sequence);
        }
        _ => {}
    }
    bytes
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum KeyWaitOutcome {
    KeyPressed,
    TimedOut,
    Shutdown,
}

fn wait_for_key_until(
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
    mut poll_key: impl FnMut(Duration) -> Result<bool>,
    mut shutdown_requested: impl FnMut() -> bool,
) -> Result<KeyWaitOutcome> {
    loop {
        if shutdown_requested() {
            return Ok(KeyWaitOutcome::Shutdown);
        }

        let Some(remaining) = deadline.checked_duration_since(now()) else {
            return Ok(KeyWaitOutcome::TimedOut);
        };
        if remaining.is_zero() {
            return Ok(KeyWaitOutcome::TimedOut);
        }

        if poll_key(remaining.min(ATTACH_POLL_INTERVAL))? {
            return Ok(KeyWaitOutcome::KeyPressed);
        }
    }
}

fn wait_for_any_key(timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let _ = wait_for_key_until(
        deadline,
        Instant::now,
        poll_and_consume_key,
        shutdown::requested,
    )?;
    Ok(())
}

fn poll_and_consume_key(timeout: Duration) -> Result<bool> {
    use crossterm::event::{self, Event, KeyEventKind};

    if !event::poll(timeout)? {
        return Ok(false);
    }
    Ok(matches!(
        event::read()?,
        Event::Key(key) if !matches!(key.kind, KeyEventKind::Release)
    ))
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

fn update_attach_log_marker(marker: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= ATTACH_LOG_MARKER_BYTES {
        marker.clear();
        marker.extend_from_slice(&bytes[bytes.len() - ATTACH_LOG_MARKER_BYTES..]);
        return;
    }

    let keep = ATTACH_LOG_MARKER_BYTES.saturating_sub(bytes.len());
    if marker.len() > keep {
        marker.drain(..marker.len() - keep);
    }
    marker.extend_from_slice(bytes);
}

/// Opens the current log path at `offset` and verifies the small byte overlap
/// immediately before it. A mismatch means the path was replaced or the file
/// was truncated and regrown between polls, so the caller resumes from a
/// bounded tail instead of replaying an arbitrarily large replacement file.
fn open_attach_log(
    path: &Path,
    offset: u64,
    expected_tail: &[u8],
) -> io::Result<(File, bool, Vec<u8>, u64)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    // A non-zero cursor without an overlap marker can happen when the initial
    // replay raced a rotation or temporarily could not open the path. Its
    // identity is unverified, so never apply that old cursor blindly to a new
    // file that happens to be at least as large.
    let identity_unverified = offset > 0 && expected_tail.is_empty();
    if len < offset || expected_tail.len() as u64 > offset || identity_unverified {
        let resume_offset = len.saturating_sub(ATTACH_REPLAY_BYTES);
        file.seek(SeekFrom::Start(resume_offset))?;
        return Ok((file, false, Vec::new(), resume_offset));
    }

    let marker_len = if expected_tail.is_empty() {
        offset.min(ATTACH_LOG_MARKER_BYTES as u64) as usize
    } else {
        expected_tail.len()
    };
    let marker_start = offset - marker_len as u64;
    file.seek(SeekFrom::Start(marker_start))?;
    let mut actual_tail = vec![0_u8; marker_len];
    file.read_exact(&mut actual_tail)?;
    let prefix_matches = expected_tail.is_empty() || actual_tail == expected_tail;
    let resume_offset = if prefix_matches {
        offset
    } else {
        len.saturating_sub(ATTACH_REPLAY_BYTES)
    };
    file.seek(SeekFrom::Start(resume_offset))?;
    Ok((file, prefix_matches, actual_tail, resume_offset))
}

fn tail_terminal_output(
    path: PathBuf,
    mut offset: u64,
    mut tail_marker: Vec<u8>,
    done: Arc<AtomicBool>,
) {
    let mut stdout = io::stdout();
    let mut buf = [0_u8; 8192];
    // Decode before writing to the user's terminal: the server may log in a
    // legacy encoding (GBK, Shift_JIS, ...), and raw legacy bytes render as
    // mojibake on a UTF-8 terminal and fail outright on a Windows console.
    let mut decoder = StreamDecoder::new();
    let mut file = None;
    let mut reopened_at = None;
    while !done.load(Ordering::Relaxed) {
        let should_reopen = reopened_at
            .is_none_or(|checked: Instant| checked.elapsed() >= ATTACH_LOG_REOPEN_INTERVAL);
        if should_reopen {
            // During log rotation the path can briefly disappear. A failed
            // open intentionally leaves an existing old handle in place;
            // the next successful open verifies its marker before switching.
            if let Ok((opened, prefix_matches, verified_tail, resume_offset)) =
                open_attach_log(&path, offset, &tail_marker)
            {
                if !prefix_matches {
                    offset = resume_offset;
                    decoder = StreamDecoder::new();
                    tail_marker.clear();
                } else {
                    tail_marker = verified_tail;
                }
                file = Some(opened);
            }
            // Missing or temporarily unreadable paths are retried at the same
            // controlled cadence as normal identity checks, not every 50 ms.
            reopened_at = Some(Instant::now());
        }
        let mut reopen = false;
        if let Some(reader) = file.as_mut() {
            let len = reader.metadata().map_or(offset, |metadata| metadata.len());
            if len < offset {
                offset = len.saturating_sub(ATTACH_REPLAY_BYTES);
                decoder = StreamDecoder::new();
                tail_marker.clear();
                if reader.seek(SeekFrom::Start(offset)).is_err() {
                    reopen = true;
                }
            }
            if !reopen {
                loop {
                    if done.load(Ordering::Relaxed) {
                        break;
                    }
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            offset = offset.saturating_add(n as u64);
                            update_attach_log_marker(&mut tail_marker, &buf[..n]);
                            let _ = stdout.write_all(decoder.push(&buf[..n]).as_bytes());
                            let _ = stdout.flush();
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => {
                            reopen = true;
                            break;
                        }
                    }
                }
            }
        }
        if reopen {
            file = None;
        }
        if done.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = stdout.write_all(decoder.flush().as_bytes());
    let _ = stdout.flush();
}

fn replay_terminal_output(path: &Path) -> Result<(u64, Vec<u8>)> {
    replay_terminal_output_to(path, &mut io::stdout())
}

fn replay_terminal_output_to(path: &Path, output: &mut impl Write) -> Result<(u64, Vec<u8>)> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok((0, Vec::new())),
    };
    let len = file.metadata()?.len();
    // Derive the tail cursor from this open handle so a path replacement
    // cannot make metadata from an earlier lookup select the wrong range.
    let mut cursor = len.saturating_sub(ATTACH_REPLAY_BYTES);
    file.seek(SeekFrom::Start(cursor))?;
    let mut buf = [0_u8; 8192];
    let mut decoder = StreamDecoder::new();
    let mut tail_marker = Vec::new();
    let mut replay = file.take(len.saturating_sub(cursor));
    loop {
        match replay.read(&mut buf)? {
            0 => break,
            n => {
                cursor = cursor.saturating_add(n as u64);
                update_attach_log_marker(&mut tail_marker, &buf[..n]);
                output.write_all(decoder.push(&buf[..n]).as_bytes())?;
            }
        }
    }
    output.write_all(decoder.flush().as_bytes())?;
    output.flush()?;
    Ok((cursor, tail_marker))
}

struct AttachTerminalGuard {
    output: Option<(Arc<AtomicBool>, thread::JoinHandle<()>)>,
}

impl AttachTerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, cursor::Show)?;
        Ok(Self { output: None })
    }

    fn start_output(&mut self, path: PathBuf, offset: u64, tail_marker: Vec<u8>) {
        let done = Arc::new(AtomicBool::new(false));
        let output_done = Arc::clone(&done);
        let output_thread =
            thread::spawn(move || tail_terminal_output(path, offset, tail_marker, output_done));
        self.output = Some((done, output_thread));
    }

    fn stop_output(&mut self) {
        if let Some((done, output_thread)) = self.output.take() {
            done.store(true, Ordering::Relaxed);
            let _ = output_thread.join();
        }
    }
}

impl Drop for AttachTerminalGuard {
    fn drop(&mut self) {
        self.stop_output();
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), cursor::Show);
    }
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
    if stop_flag_path(store, &server).exists() {
        cleanup_runtime(store, &server);
        return Ok(());
    }

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

    let command_file = command_path(store, server);
    fs::write(&command_file, b"")?;
    let mut command_reader = File::open(&command_file)?;

    // Seed the PTY at the viewing terminal's size (published by
    // start_supervisor, or left over from the last attach) so JLine formats
    // its output for the right width from the very first line. Falls back to a
    // sane default for headless starts with no terminal on record.
    let (initial_rows, initial_cols) = initial_pty_size(store, server);
    let (master, slave_fd) = open_pty(initial_rows, initial_cols)?;
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
    let resize_file = resize_path(store, server);
    let mut master_writer = master.try_clone()?;
    let command_thread = thread::spawn(move || {
        let mut size = None;
        let mut resize_checked_at: Option<Instant> = None;
        let mut command_buf = Vec::new();
        let mut command_written = 0;
        while !command_done.load(Ordering::Relaxed) {
            let command_result = pump_terminal_commands(
                &mut command_reader,
                &mut command_buf,
                &mut command_written,
                &mut master_writer,
            );
            if command_result.is_err() {
                break;
            }
            let now = Instant::now();
            if resize_checked_at
                .is_none_or(|checked| now.duration_since(checked) >= RESIZE_POLL_INTERVAL)
            {
                let _ = pump_terminal_resize(&resize_file, &master_writer, &mut size);
                resize_checked_at = Some(now);
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

    let command_file = command_path(store, server);
    fs::write(&command_file, b"")?;
    let mut command_reader = File::open(&command_file)?;
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
    let mut stdin = child.stdin.take().context("open child stdin")?;
    let command_thread = thread::spawn(move || {
        let mut command_buf = Vec::new();
        let mut command_written = 0;
        while !command_done.load(Ordering::Relaxed) {
            let command_result = pump_commands(
                &mut command_reader,
                &mut command_buf,
                &mut command_written,
                &mut stdin,
            );
            if command_result.is_err() {
                break;
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
                if let Some((sequence, consumed)) = read_allowed_input_escape_bytes(&input[index..])
                {
                    output.extend_from_slice(sequence);
                    index += consumed;
                } else if input
                    .get(index + 1)
                    .is_none_or(|next| !matches!(*next, b'[' | b']' | b'O' | b'(' | b')'))
                {
                    // A standalone Escape and Alt+text are legitimate
                    // terminal input. Structured CSI/OSC/SS3 sequences still
                    // go through the allowlist above (or are dropped below).
                    output.push(0x1b);
                    index += 1;
                } else {
                    index += skip_escape_bytes(&input[index..]).max(1);
                }
            }
            0x08 => {
                output.push(0x7f);
                index += 1;
            }
            0x00..=0x1a | 0x1c..=0x1f | 0x7f => {
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
        }
    }

    output
}

fn read_allowed_input_escape_bytes(input: &[u8]) -> Option<(&[u8], usize)> {
    match input {
        [0x1b, b'[', rest @ ..] => {
            let end = rest.iter().position(|byte| (b'@'..=b'~').contains(byte))?;
            let consumed = 2 + end + 1;
            if consumed > 32 {
                return None;
            }
            let sequence = &input[..consumed];
            is_allowed_csi_input(sequence).then_some((sequence, consumed))
        }
        [0x1b, b'O', ch, ..] if matches!(*ch, b'A'..=b'D' | b'F' | b'H' | b'P'..=b'S') => {
            Some((&input[..3], 3))
        }
        _ => None,
    }
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

fn is_allowed_csi_input(sequence: &[u8]) -> bool {
    let Some(body) = sequence.strip_prefix(b"\x1b[") else {
        return false;
    };
    let Some(final_char) = body.last().copied() else {
        return false;
    };
    if !(b'@'..=b'~').contains(&final_char) {
        return false;
    }
    let prefix_len = body.len().saturating_sub(1);
    if matches!(final_char, b'M' | b'm') && body.starts_with(b"<") {
        return false;
    }
    body[..prefix_len].iter().all(|byte| {
        byte.is_ascii_digit() || matches!(*byte, b';' | b'?' | b'>' | b'<' | b':' | b' ')
    })
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
fn pump_commands(
    file: &mut File,
    buf: &mut Vec<u8>,
    written: &mut usize,
    stdin: &mut impl Write,
) -> Result<()> {
    if *written == buf.len() {
        buf.clear();
        *written = 0;
        file.read_to_end(buf)?;
    }
    write_pending(buf, written, stdin)
}

#[cfg(unix)]
fn pump_terminal_commands(
    file: &mut File,
    buf: &mut Vec<u8>,
    written: &mut usize,
    terminal: &mut impl Write,
) -> Result<()> {
    if *written == buf.len() {
        buf.clear();
        *written = 0;
        let read_result = file.read_to_end(buf);
        for byte in &mut *buf {
            if *byte == b'\n' {
                *byte = b'\r';
            }
        }
        read_result?;
    }
    write_pending(buf, written, terminal)
}

fn write_pending(buf: &[u8], written: &mut usize, output: &mut impl Write) -> Result<()> {
    while *written < buf.len() {
        match output.write(&buf[*written..]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
            Ok(count) => *written += count,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) => return Err(err.into()),
        }
    }
    if !buf.is_empty() {
        output.flush()?;
    }
    Ok(())
}

#[cfg(unix)]
fn pump_terminal_resize(
    path: &Path,
    terminal: &File,
    last_size: &mut Option<(u16, u16)>,
) -> Result<()> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
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

/// The PTY size used when no viewing terminal size has been recorded yet
/// (headless `remiaft start` with stdout redirected, for example).
#[cfg(unix)]
const DEFAULT_PTY_SIZE: (u16, u16) = (40, 160);

/// Reads the viewing terminal size published to the resize file, falling back
/// to [`DEFAULT_PTY_SIZE`] when it is absent or unparsable. Returns
/// `(rows, cols)` to match the resize file's field order.
#[cfg(unix)]
fn initial_pty_size(store: &ConfigStore, server: &ServerConfig) -> (u16, u16) {
    let Ok(raw) = fs::read_to_string(resize_path(store, server)) else {
        return DEFAULT_PTY_SIZE;
    };
    let mut parts = raw.split_whitespace();
    let rows = parts.next().and_then(|value| value.parse::<u16>().ok());
    let cols = parts.next().and_then(|value| value.parse::<u16>().ok());
    match (rows, cols) {
        (Some(rows), Some(cols)) => (rows.max(1), cols.max(1)),
        _ => DEFAULT_PTY_SIZE,
    }
}

#[cfg(unix)]
fn open_pty(rows: u16, cols: u16) -> Result<(File, i32)> {
    let mut master_fd = 0;
    let mut slave_fd = 0;
    #[cfg(target_vendor = "apple")]
    let rc = {
        let mut size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
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
            ws_row: rows,
            ws_col: cols,
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
    termios.c_lflag &= !(libc::ECHO | libc::ECHONL);
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
    match fs::read_to_string(path) {
        Ok(raw) => Ok(raw.trim().parse::<u32>().ok()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
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
    // Signal 0 sends nothing but still validates the pid exists; this avoids
    // spawning a `kill` subprocess on every status check (called once per
    // configured server on every redraw).
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let alive =
            GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code == STILL_ACTIVE as u32;
        CloseHandle(handle);
        alive
    }
}

#[cfg(unix)]
fn kill_pid(pid: u32) -> Result<()> {
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err).context("send SIGTERM");
        }
    }
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
    use std::cell::Cell;

    use super::*;

    #[test]
    fn key_wait_returns_as_soon_as_a_key_is_ready() {
        let start = Instant::now();
        let polls = Cell::new(0);

        let outcome = wait_for_key_until(
            start + CONSOLE_EXIT_RETURN_TIMEOUT,
            || start,
            |timeout| {
                assert_eq!(timeout, ATTACH_POLL_INTERVAL);
                polls.set(polls.get() + 1);
                Ok(true)
            },
            || false,
        )
        .unwrap();

        assert_eq!(outcome, KeyWaitOutcome::KeyPressed);
        assert_eq!(polls.get(), 1);
    }

    #[test]
    fn key_wait_caps_the_last_poll_at_the_deadline() {
        let start = Instant::now();
        let clock = Cell::new(start);
        let polls = Cell::new(0);
        let expected_waits = [
            ATTACH_POLL_INTERVAL,
            ATTACH_POLL_INTERVAL,
            Duration::from_millis(50),
        ];

        let outcome = wait_for_key_until(
            start + Duration::from_millis(450),
            || clock.get(),
            |timeout| {
                assert_eq!(timeout, expected_waits[polls.get()]);
                polls.set(polls.get() + 1);
                clock.set(clock.get() + timeout);
                Ok(false)
            },
            || false,
        )
        .unwrap();

        assert_eq!(outcome, KeyWaitOutcome::TimedOut);
        assert_eq!(polls.get(), expected_waits.len());
    }

    #[test]
    fn key_wait_honors_shutdown_between_input_polls() {
        let start = Instant::now();
        let polls = Cell::new(0);

        let outcome = wait_for_key_until(
            start + CONSOLE_EXIT_RETURN_TIMEOUT,
            || start,
            |_| {
                polls.set(polls.get() + 1);
                Ok(false)
            },
            || polls.get() > 0,
        )
        .unwrap();

        assert_eq!(outcome, KeyWaitOutcome::Shutdown);
        assert_eq!(polls.get(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn terminal_command_reader_keeps_its_cursor_between_appends() {
        let directory =
            std::env::temp_dir().join(format!("remiaft-command-reader-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("commands.in");
        fs::write(&path, b"say one\n").unwrap();

        let mut reader = File::open(&path).unwrap();
        let mut read_buf = Vec::new();
        let mut written = 0;
        let mut terminal = Vec::new();
        pump_terminal_commands(&mut reader, &mut read_buf, &mut written, &mut terminal).unwrap();
        assert_eq!(terminal, b"say one\r");

        pump_terminal_commands(&mut reader, &mut read_buf, &mut written, &mut terminal).unwrap();
        assert_eq!(terminal, b"say one\r");

        let mut writer = OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"say two\n").unwrap();
        writer.flush().unwrap();
        pump_terminal_commands(&mut reader, &mut read_buf, &mut written, &mut terminal).unwrap();
        assert_eq!(terminal, b"say one\rsay two\r");

        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn pending_command_bytes_resume_after_would_block() {
        struct WouldBlockOnce {
            phase: u8,
            bytes: Vec<u8>,
        }

        impl Write for WouldBlockOnce {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                match self.phase {
                    0 => {
                        let count = buf.len().min(2);
                        self.bytes.extend_from_slice(&buf[..count]);
                        self.phase = 1;
                        Ok(count)
                    }
                    1 => {
                        self.phase = 2;
                        Err(io::Error::from(io::ErrorKind::WouldBlock))
                    }
                    _ => {
                        self.bytes.extend_from_slice(buf);
                        Ok(buf.len())
                    }
                }
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut output = WouldBlockOnce {
            phase: 0,
            bytes: Vec::new(),
        };
        let mut written = 0;
        write_pending(b"abcdef", &mut written, &mut output).unwrap();
        assert_eq!(written, 2);
        assert_eq!(output.bytes, b"ab");

        write_pending(b"abcdef", &mut written, &mut output).unwrap();
        assert_eq!(written, 6);
        assert_eq!(output.bytes, b"abcdef");
    }

    #[test]
    fn attach_log_reopen_verifies_its_previous_tail() {
        let directory =
            std::env::temp_dir().join(format!("remiaft-attach-log-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("minecraft.log");
        fs::write(&path, b"abcdef").unwrap();

        let expected_marker = b"abcdef";
        let (_, prefix_matches, mut marker, _) =
            open_attach_log(&path, 6, expected_marker).unwrap();
        assert!(prefix_matches);
        assert_eq!(marker, b"abcdef");

        let mut writer = OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"gh").unwrap();
        writer.flush().unwrap();
        let (mut reader, prefix_matches, _, _) = open_attach_log(&path, 6, &marker).unwrap();
        assert!(prefix_matches);
        let mut appended = Vec::new();
        reader.read_to_end(&mut appended).unwrap();
        assert_eq!(appended, b"gh");
        update_attach_log_marker(&mut marker, &appended);

        drop(reader);
        drop(writer);
        let replacement_len = ATTACH_REPLAY_BYTES as usize + 10;
        fs::write(&path, vec![b'x'; replacement_len]).unwrap();
        let (_, prefix_matches, _, resume_offset) = open_attach_log(&path, 8, &marker).unwrap();
        assert!(!prefix_matches);
        assert_eq!(resume_offset, 10);

        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn attach_log_does_not_trust_a_nonzero_cursor_without_a_marker() {
        let directory = std::env::temp_dir().join(format!(
            "remiaft-attach-unverified-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("minecraft.log");
        let replacement_len = ATTACH_REPLAY_BYTES as usize + 20;
        fs::write(&path, vec![b'x'; replacement_len]).unwrap();

        let (_, prefix_matches, marker, resume_offset) = open_attach_log(&path, 10, &[]).unwrap();
        assert!(!prefix_matches);
        assert!(marker.is_empty());
        assert_eq!(resume_offset, 20);

        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn attach_tail_starts_after_every_byte_replayed() {
        let directory =
            std::env::temp_dir().join(format!("remiaft-attach-replay-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("minecraft.log");
        fs::write(&path, b"history").unwrap();

        let mut replayed = Vec::new();
        let (replay_end, marker) = replay_terminal_output_to(&path, &mut replayed).unwrap();
        assert_eq!(replayed, b"history");
        assert_eq!(replay_end, 7);
        assert_eq!(marker, b"history");

        let mut writer = OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"new").unwrap();
        writer.flush().unwrap();
        let (mut reader, prefix_matches, _, resume_offset) =
            open_attach_log(&path, replay_end, &marker).unwrap();
        assert!(prefix_matches);
        assert_eq!(resume_offset, replay_end);
        let mut appended = Vec::new();
        reader.read_to_end(&mut appended).unwrap();
        assert_eq!(appended, b"new");

        drop(reader);
        drop(writer);
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn attach_replay_is_bounded_and_keeps_a_rolling_marker() {
        let directory = std::env::temp_dir().join(format!(
            "remiaft-attach-bounded-replay-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("minecraft.log");
        let len = ATTACH_REPLAY_BYTES as usize + 10;
        let bytes = (0..len)
            .map(|index| b'a' + (index % 26) as u8)
            .collect::<Vec<_>>();
        fs::write(&path, &bytes).unwrap();

        let mut replayed = Vec::new();
        let (replay_end, marker) = replay_terminal_output_to(&path, &mut replayed).unwrap();
        assert_eq!(replay_end, len as u64);
        assert_eq!(replayed, bytes[10..]);
        assert_eq!(marker, bytes[len - ATTACH_LOG_MARKER_BYTES..]);

        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn windows_keys_are_encoded_as_terminal_input() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        assert_eq!(
            windows_key_bytes(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            [0x03]
        );
        assert_eq!(
            windows_key_bytes(KeyEvent::new(KeyCode::Char('服'), KeyModifiers::NONE)),
            "服".as_bytes()
        );
        assert_eq!(
            windows_key_bytes(KeyEvent::new(
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
            b"@"
        );
        assert_eq!(
            windows_key_bytes(KeyEvent::new(
                KeyCode::Char('\u{20ac}'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
            "\u{20ac}".as_bytes()
        );
        assert_eq!(
            windows_key_bytes(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            b"\x1b[D"
        );
        assert_eq!(
            sanitize_terminal_bytes(&windows_key_bytes(KeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::ALT,
            ))),
            b"\x1bb"
        );
        assert_eq!(
            sanitize_terminal_bytes(&windows_key_bytes(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            ))),
            [0x1b]
        );
        assert_eq!(
            sanitize_terminal_bytes(&windows_key_bytes(KeyEvent::new(
                KeyCode::Char(' '),
                KeyModifiers::CONTROL,
            ))),
            [0x00]
        );
    }

    #[test]
    fn terminal_guard_stops_output_on_drop() {
        let done = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let output_done = Arc::clone(&done);
        let output_exited = Arc::clone(&exited);
        let guard = AttachTerminalGuard {
            output: Some((
                done,
                thread::spawn(move || {
                    while !output_done.load(Ordering::Relaxed) {
                        thread::yield_now();
                    }
                    output_exited.store(true, Ordering::Relaxed);
                }),
            )),
        };

        drop(guard);

        assert!(exited.load(Ordering::Relaxed));
    }

    fn sanitize_text(input: &str) -> String {
        String::from_utf8_lossy(&sanitize_terminal_bytes(input.as_bytes())).into_owned()
    }

    #[test]
    fn terminal_input_normalizes_backspace_to_delete() {
        assert_eq!(sanitize_text("sa\u{8}y\r"), "sa\u{7f}y\r");
    }

    #[test]
    fn terminal_input_keeps_known_editing_keys() {
        assert_eq!(
            sanitize_text(
                "\u{1b}[A\u{1b}[B\u{1b}[C\u{1b}[D\u{1b}[H\u{1b}[F\u{1b}[3~\u{1}\u{5}\u{15}\t"
            ),
            "\u{1b}[A\u{1b}[B\u{1b}[C\u{1b}[D\u{1b}[H\u{1b}[F\u{1b}[3~\u{1}\u{5}\u{15}\t"
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
    fn terminal_input_drops_minecraft_formatting_marker() {
        assert_eq!(sanitize_text("say \u{a7}red\u{7f}\r"), "say red\u{7f}\r");
    }
}
