//! Ferric Commander — a dual-pane terminal file manager.
//!
//! Architecture in one paragraph: there is no async runtime. The main thread owns
//! all state and renders on a 16 ms budget; anything that can block for seconds
//! (copying, moving, deleting) runs on a `std::thread` that talks back over an
//! `mpsc` channel. That split is what keeps the UI at 60 FPS during a
//! multi-gigabyte transfer while avoiding `Arc<Mutex<..>>` entirely.
//!
//! This file owns the process-level concerns only: putting the terminal into and
//! out of raw mode safely, installing a panic hook that cannot leave the user's
//! terminal wedged, and running the event loop.

mod app;
mod fs;
mod theme;
mod ui;

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::App;

/// ~60 FPS: the frame budget, and also the longest the loop sleeps for input.
const TICK_RATE: Duration = Duration::from_millis(16);

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn main() -> Result<()> {
    // The guard is created before anything else can fail, so every early return
    // below leaves a terminal that `Drop` will restore.
    let mut guard = TerminalGuard::init()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("cannot initialise the terminal backend")?;
    terminal.clear()?;

    let mut app = App::new()?;
    let result = run_app(&mut terminal, &mut app);

    // Restore before returning so `anyhow`'s error report is printed on a normal
    // screen rather than into the alternate buffer, where it would vanish.
    guard.suspend()?;

    result
}

/// Owns the terminal's raw mode and alternate screen.
///
/// Two independent safety nets, because either one alone has a hole: `Drop`
/// covers normal returns and `?` early exits, while the panic hook covers
/// panics, where unwinding may abort before `Drop` runs. Restoring twice is
/// harmless — both paths are idempotent and ignore errors.
struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn init() -> Result<Self> {
        Self::install_panic_hook();
        Self::enter()?;
        Ok(Self { active: true })
    }

    /// Install a hook that restores the terminal, then chains to the previous
    /// hook so the panic message and backtrace still print normally.
    fn install_panic_hook() {
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = Self::leave();
            original_hook(panic_info);
        }));
    }

    fn enter() -> Result<()> {
        terminal::enable_raw_mode().context("cannot enable raw mode")?;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)
            .context("cannot enter the alternate screen")?;
        Ok(())
    }

    fn leave() -> Result<()> {
        // Order matters: the cursor must be shown again before raw mode is
        // disabled, or some terminals swallow the escape sequence.
        execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)?;
        terminal::disable_raw_mode()?;
        Ok(())
    }

    /// Hand the terminal back to the shell, e.g. to run an external editor.
    fn suspend(&mut self) -> Result<()> {
        if self.active {
            Self::leave()?;
            self.active = false;
        }
        Ok(())
    }

    /// Reclaim the terminal after a `suspend`.
    ///
    /// Not called yet: Enter-on-file launches `$EDITOR` in Phase 2. It lives
    /// here so the lifecycle is defined in one place rather than duplicated at
    /// the call site.
    #[allow(dead_code)]
    fn resume(&mut self) -> Result<()> {
        if !self.active {
            Self::enter()?;
            self.active = true;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = Self::leave();
    }
}

/// The event loop.
///
/// Order per iteration is deliberate: draw, then block on input for the
/// remainder of the frame budget. Drawing first means the worst-case key latency
/// is one frame, and polling last means an idle app sleeps instead of spinning.
/// Phase 4 inserts `app.pump_worker(..)` before the draw.
fn run_app(terminal: &mut Tui, app: &mut App) -> Result<()> {
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        let budget = TICK_RATE.saturating_sub(last_tick.elapsed());
        if event::poll(budget)? {
            // Filtering on `Press` is essential: terminals with the Kitty
            // keyboard protocol report Release and Repeat too, which would
            // otherwise fire every action two or three times.
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                app.handle_key_event(key);
            }
            // Resize needs no handling: ratatui observes the new area on the
            // next `draw`, because the backend resizes the buffer every frame.
        }

        if last_tick.elapsed() >= TICK_RATE {
            last_tick = Instant::now();
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}
