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
mod format;
mod fs;
mod tasks;
mod theme;
mod ui;

use std::io::{self, Stdout};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{App, Effect};

/// ~60 FPS: the frame budget, and also the longest the loop sleeps for input.
const TICK_RATE: Duration = Duration::from_millis(16);

/// Worker messages consumed per frame. Identical to the specification's 100:
/// enough to keep a progress bar current at 60 FPS, bounded so a worker that
/// queues thousands of updates cannot starve the renderer.
const MAX_UPDATES_PER_FRAME: usize = 100;

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn main() -> Result<()> {
    // The guard is created before anything else can fail, so every early return
    // below leaves a terminal that `Drop` will restore.
    let mut guard = TerminalGuard::init()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("cannot initialise the terminal backend")?;
    terminal.clear()?;

    let mut app = App::new()?;
    let result = run_app(&mut terminal, &mut app, &mut guard);

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
        // Only if we still hold the terminal: after `suspend` the shell owns it
        // again, and emitting a second LeaveAlternateScreen there is a no-op on
        // most terminals but noise on some.
        if self.active {
            let _ = Self::leave();
        }
    }
}

/// The event loop.
///
/// Order per iteration is deliberate: draw, then block on input for the
/// remainder of the frame budget. Drawing first means the worst-case key latency
/// is one frame, and polling last means an idle app sleeps instead of spinning.
/// It also owns the worker pump, which must happen before the draw so a frame
/// never renders state that is one message stale.
fn run_app(terminal: &mut Tui, app: &mut App, guard: &mut TerminalGuard) -> Result<()> {
    let mut last_tick = Instant::now();

    loop {
        // Drain worker output first: the frame below should show the newest
        // state that arrived since the previous one.
        app.pump_worker(MAX_UPDATES_PER_FRAME);

        terminal.draw(|frame| ui::render(frame, app))?;

        let budget = TICK_RATE.saturating_sub(last_tick.elapsed());
        if event::poll(budget)? {
            // Filtering on `Press` is essential: terminals with the Kitty
            // keyboard protocol report Release and Repeat too, which would
            // otherwise fire every action two or three times.
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                // Key handling itself never touches the terminal; anything that
                // needs the screen comes back as an `Effect` to run here.
                if let Some(effect) = app.handle_key_event(key) {
                    apply_effect(guard, terminal, app, effect)?;
                }
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

/// Execute an effect the app could not carry out itself.
fn apply_effect(
    guard: &mut TerminalGuard,
    terminal: &mut Tui,
    app: &mut App,
    effect: Effect,
) -> Result<()> {
    match effect {
        Effect::OpenInEditor(path) => open_in_editor(guard, terminal, app, &path),
    }
}

/// Resolve the editor to run: `$EDITOR`, then `$VISUAL`, then `vi`.
///
/// Split out from [`open_in_editor`] so the precedence rules are testable
/// without mutating the process environment (which is global, and racy under
/// the parallel test runner).
///
/// Only whitespace splitting is supported, so `EDITOR="code -w"` works but
/// `EDITOR="'/opt/My Editor/bin/x'"` does not. Quoting rules would need a real
/// shell-word parser; anyone who needs that should point `$EDITOR` at a small
/// wrapper script instead, which is the usual Unix answer.
fn editor_command(editor: Option<String>, visual: Option<String>) -> (String, Vec<String>) {
    // Blank must be normalised *before* the fallback chain, not after: an
    // exported-but-empty EDITOR would otherwise win the `or` and silently
    // shadow a perfectly good VISUAL.
    let clean = |value: Option<String>| {
        value
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };

    let raw = clean(editor)
        .or_else(|| clean(visual))
        .unwrap_or_else(|| "vi".to_string());

    let mut parts = raw.split_whitespace();
    let program = parts.next().unwrap_or("vi").to_string();
    (program, parts.map(str::to_string).collect())
}

/// Launch the editor on `path`, giving it the terminal and taking it back.
///
/// The suspend/restore pair is unconditional: a failed spawn, a crash inside
/// the editor, and a clean exit all leave the TUI in the same working state.
/// `terminal.clear()` afterwards is not cosmetic — ratatui's buffer still
/// believes the old frame is on screen, and the editor has painted over it, so
/// without the clear the next draw would only emit a diff against a stale
/// picture and leave the editor's output smeared through the UI.
fn open_in_editor(
    guard: &mut TerminalGuard,
    terminal: &mut Tui,
    app: &mut App,
    path: &Path,
) -> Result<()> {
    let (program, args) =
        editor_command(std::env::var("EDITOR").ok(), std::env::var("VISUAL").ok());

    guard.suspend()?;
    let status = Command::new(&program).args(&args).arg(path).status();
    guard.resume()?;
    terminal.clear()?;

    // The editor may have renamed, truncated or deleted the file, so both panes
    // are re-read rather than patched.
    app.refresh_both_panels();

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    app.status_msg = Some(match status {
        Ok(exit) if exit.success() => format!("Closed {name}"),
        Ok(exit) => match exit.code() {
            Some(code) => format!("{program} exited with status {code}: {name}"),
            None => format!("{program} was killed by a signal: {name}"),
        },
        Err(err) => format!("cannot launch {program}: {err}"),
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_precedence_is_editor_then_visual_then_vi() {
        assert_eq!(
            editor_command(Some("nvim".into()), Some("vim".into())).0,
            "nvim"
        );
        assert_eq!(editor_command(None, Some("vim".into())).0, "vim");
        assert_eq!(editor_command(None, None).0, "vi");
    }

    #[test]
    fn blank_editor_variable_falls_through() {
        // An exported-but-empty EDITOR is common in container images; treated
        // as unset rather than as "run a program with an empty name".
        assert_eq!(editor_command(Some(String::new()), None).0, "vi");
        assert_eq!(editor_command(Some("   ".into()), None).0, "vi");
        assert_eq!(
            editor_command(Some(String::new()), Some("nano".into())).0,
            "nano"
        );
    }

    #[test]
    fn editor_arguments_are_split_off_the_program() {
        assert_eq!(
            editor_command(Some("code -w --new-window".into()), None),
            (
                "code".to_string(),
                vec!["-w".to_string(), "--new-window".to_string()]
            )
        );
    }
}
