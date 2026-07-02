//! Crash/kill-proofing for the interactive terminal.
//!
//! `remiaft` puts the terminal into raw mode and the alternate screen while
//! the TUI or the native console attach is active. If the process is killed
//! externally (SIGTERM/SIGHUP, a Windows console close/logoff/shutdown
//! event) or panics, those Drop-based restorations never run and the user's
//! real terminal is left unusable until they run `reset` or open a new
//! window. `install()` wires up a panic hook and best-effort OS-level
//! handlers so the terminal is restored whenever the process gets any
//! chance at all to react. SIGKILL and forceful task termination can never
//! be intercepted by any process - that is an OS-level guarantee, not a gap
//! in this code.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor;
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// True once an external signal/ctrl event asked the process to exit.
/// Event loops should check this alongside their normal quit key and exit
/// through their usual clean shutdown path.
pub fn requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

fn request() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// Best-effort terminal restoration. Safe to call from a panic hook or a
/// signal/ctrl handler: every step ignores its own errors rather than
/// panicking or blocking.
pub fn force_restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}

/// Install the panic hook and platform shutdown handlers. Call once, early
/// in `main`, before any raw-mode/alternate-screen terminal state is set up.
pub fn install() {
    install_panic_hook();
    install_platform_handlers();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        force_restore_terminal();
        previous(info);
    }));
}

#[cfg(unix)]
fn install_platform_handlers() {
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            libc::signal(signal, handle_unix_signal as libc::sighandler_t);
        }
    }
}

#[cfg(unix)]
extern "C" fn handle_unix_signal(_signal: libc::c_int) {
    request();
}

#[cfg(windows)]
fn install_platform_handlers() {
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(handle_console_event), 1);
    }
}

#[cfg(windows)]
unsafe extern "system" fn handle_console_event(ctrl_type: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    request();
    // The process may be torn down immediately after this handler returns
    // for close/logoff/shutdown events, so restore synchronously here rather
    // than relying on the main loop noticing the flag on its next tick.
    if matches!(
        ctrl_type,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT | CTRL_BREAK_EVENT
    ) {
        force_restore_terminal();
    }
    1
}

#[cfg(not(any(unix, windows)))]
fn install_platform_handlers() {}
