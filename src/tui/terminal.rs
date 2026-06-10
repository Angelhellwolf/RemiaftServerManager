use std::io::{self, Stdout, Write};

use anyhow::Result;
use crossterm::cursor;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear as TerminalClear, ClearType, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Frame, Terminal};

pub(super) struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    pub(super) fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    pub(super) fn draw<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.terminal.draw(f)?;
        Ok(())
    }

    pub(super) fn clear(&mut self) -> Result<()> {
        self.terminal.clear()?;
        Ok(())
    }

    pub(super) fn size(&self) -> Result<Rect> {
        let size = self.terminal.size()?;
        Ok(Rect::new(0, 0, size.width, size.height))
    }

    pub(super) fn suspend(&mut self) -> Result<()> {
        self.terminal.show_cursor()?;
        self.terminal.clear()?;
        execute!(
            self.terminal.backend_mut(),
            TerminalClear(ClearType::All),
            cursor::MoveTo(0, 0),
            LeaveAlternateScreen
        )?;
        self.terminal.backend_mut().flush()?;
        disable_raw_mode()?;
        Ok(())
    }

    pub(super) fn resume(&mut self) -> Result<()> {
        enable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            TerminalClear(ClearType::All),
            cursor::MoveTo(0, 0)
        )?;
        self.terminal.hide_cursor()?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = self.terminal.clear();
        let _ = execute!(
            self.terminal.backend_mut(),
            TerminalClear(ClearType::All),
            cursor::MoveTo(0, 0),
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}
