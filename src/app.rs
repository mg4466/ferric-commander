//! Application state machine and keyboard routing.
//!
//! `App` owns every piece of mutable state. The renderer borrows it immutably,
//! the event loop borrows it mutably, and background workers never touch it:
//! they communicate only through channels, which the Phase 4 `pump_worker` will
//! drain. Keeping the ownership single-threaded is what removes the need for
//! `Arc<Mutex<..>>` anywhere in this project.
//!
//! Key handling is written as an explicit match rather than a lookup table for
//! now. Phase 7 replaces the match with a `KeyChord -> Action` map loaded from
//! `keymap.conf`; the dispatch structure below is already shaped for that split,
//! since every arm calls a small named method instead of doing work inline.

use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::fs::panel::PanelState;
use crate::theme::{Theme, ThemeType};

/// Something the app cannot do by itself because it needs the terminal.
///
/// `App` is deliberately terminal-agnostic: it never touches raw mode or the
/// alternate screen. Actions that require handing the screen to another process
/// are returned to the event loop as an `Effect`, which owns the
/// suspend/restore dance. That keeps `App` fully testable without a real TTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Suspend the TUI, run `$EDITOR` on this path, then resume and refresh.
    OpenInEditor(PathBuf),
}

/// Which pane receives navigation keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePane {
    Left,
    Right,
}

impl ActivePane {
    fn flipped(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// Everything the UI needs for one frame.
#[derive(Debug)]
pub struct App {
    pub left_panel: PanelState,
    pub right_panel: PanelState,
    pub active_pane: ActivePane,
    pub theme_type: ThemeType,
    /// Resolved colours for `theme_type`. Kept in sync by `cycle_theme` so the
    /// renderer never has to re-resolve a palette per widget.
    pub theme: Theme,
    /// Transient one-line message (an error, or a confirmation of what just
    /// happened). Phase 3 routes hard errors to a modal instead.
    pub status_msg: Option<String>,
    pub should_quit: bool,
}

impl App {
    /// Build the application with both panes rooted at the process cwd.
    ///
    /// Fails only when the cwd cannot be read at all, which is unrecoverable
    /// for a two-pane file manager.
    pub fn new() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let theme_type = ThemeType::CatppuccinMocha;

        Ok(Self {
            left_panel: PanelState::new(cwd.clone())?,
            right_panel: PanelState::new(cwd)?,
            active_pane: ActivePane::Left,
            theme_type,
            theme: theme_type.palette(),
            status_msg: None,
            should_quit: false,
        })
    }

    /// The pane that keyboard navigation applies to.
    pub fn active_panel(&self) -> &PanelState {
        match self.active_pane {
            ActivePane::Left => &self.left_panel,
            ActivePane::Right => &self.right_panel,
        }
    }

    /// Mutable access to the focused pane.
    pub fn active_panel_mut(&mut self) -> &mut PanelState {
        match self.active_pane {
            ActivePane::Left => &mut self.left_panel,
            ActivePane::Right => &mut self.right_panel,
        }
    }

    /// The pane that is *not* focused, i.e. the default destination of copy
    /// and move. Exposed as a method so Phase 4 cannot accidentally read the
    /// focused pane when it meant the other one.
    #[allow(dead_code)] // Consumed by the F5/F6 handlers in Phase 4.
    pub fn inactive_panel(&self) -> &PanelState {
        match self.active_pane {
            ActivePane::Left => &self.right_panel,
            ActivePane::Right => &self.left_panel,
        }
    }

    /// Swap focus between panes.
    pub fn switch_pane(&mut self) {
        self.active_pane = self.active_pane.flipped();
    }

    /// Re-read both panes from disk. Used after any operation that changed the
    /// filesystem, and after returning from an external editor.
    #[allow(dead_code)] // Called once worker completions exist (Phase 4).
    pub fn refresh_both_panels(&mut self) {
        if let Err(err) = self.left_panel.refresh() {
            self.set_error(err);
        }
        if let Err(err) = self.right_panel.refresh() {
            self.set_error(err);
        }
    }

    /// Replace the status line with a human-readable error.
    ///
    /// `{:#}` prints the whole `anyhow` chain on one line ("cannot open /root:
    /// Permission denied (os error 13)") without the multi-line `Debug` form.
    fn set_error(&mut self, err: anyhow::Error) {
        self.status_msg = Some(format!("{err:#}"));
    }

    fn clear_status(&mut self) {
        if self.status_msg.is_some() {
            self.status_msg = None;
        }
    }

    /// Advance the palette cycle and keep the resolved theme in step.
    pub fn cycle_theme(&mut self) {
        self.theme_type = self.theme_type.next();
        self.theme = self.theme_type.palette();
        self.status_msg = Some(format!("Theme: {}", self.theme.name));
    }

    /// Route one key press. Only `Press` events reach here: crossterm also
    /// reports Release/Repeat on Windows and on enhanced terminals, and acting
    /// on those would fire every action twice.
    /// Returns an [`Effect`] when the key requires the terminal (see `main.rs`).
    pub fn handle_key_event(&mut self, key: KeyEvent) -> Option<Effect> {
        // Ctrl+C is a reflex for killing a misbehaving TUI; honour it even
        // though raw mode means the terminal will not generate SIGINT.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return None;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::F(10) => {
                self.should_quit = true;
                None
            }

            KeyCode::Tab => {
                self.switch_pane();
                None
            }

            KeyCode::Up | KeyCode::Char('k') => {
                self.clear_status();
                self.active_panel_mut().move_up();
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.clear_status();
                self.active_panel_mut().move_down();
                None
            }

            KeyCode::Enter => self.activate_cursor(),
            KeyCode::Backspace => {
                self.go_parent();
                None
            }

            KeyCode::Char(' ') | KeyCode::Insert => {
                self.active_panel_mut().toggle_select();
                None
            }
            KeyCode::Char('a') => {
                self.active_panel_mut().invert_selection();
                None
            }

            KeyCode::F(1) | KeyCode::Char('t') => {
                self.cycle_theme();
                None
            }
            KeyCode::F(2) | KeyCode::Char('s') => {
                self.active_panel_mut().cycle_sort();
                let panel = self.active_panel();
                self.status_msg = Some(format!(
                    "Sort: {} {}",
                    panel.sort_col.label(),
                    if panel.sort_asc { "Asc" } else { "Desc" }
                ));
                None
            }

            // Everything else is unbound for now. Swallowing it (rather than
            // falling through to a default action) is what keeps a stray key
            // from doing something destructive.
            _ => None,
        }
    }

    /// Act on the row under the cursor: the single entry point for Enter, so
    /// the "what is this thing" decision lives in exactly one place.
    ///
    /// Directories and ".." change the pane's own state. A regular file is not
    /// something this app should silently ignore — it hands back an
    /// `Effect::OpenInEditor` and lets the event loop deal with the terminal.
    fn activate_cursor(&mut self) -> Option<Effect> {
        let entry = self.active_panel().selected_entry()?;

        // Decide from borrowed data, then act: the panel is borrowed here, so
        // the clone of the path is what lets `self` be mutated below.
        let path = entry.path.clone();
        let is_parent_nav = entry.is_parent_nav;
        let is_dir = entry.is_dir;
        let is_broken = entry.is_broken_symlink;

        if is_parent_nav {
            self.go_parent();
            return None;
        }

        if is_broken {
            self.status_msg = Some(format!("{path:?} is a broken symlink"));
            return None;
        }

        if is_dir {
            self.enter_directory();
            return None;
        }

        self.clear_status();
        Some(Effect::OpenInEditor(path))
    }

    /// Enter a directory, surfacing a failure on the status line without
    /// moving the panel.
    fn enter_directory(&mut self) {
        self.clear_status();
        if let Err(err) = self.active_panel_mut().enter_directory() {
            self.set_error(err);
        }
    }

    /// Go up one level, surfacing a failure without moving the panel.
    fn go_parent(&mut self) {
        self.clear_status();
        if let Err(err) = self.active_panel_mut().go_parent() {
            self.set_error(err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app_at_tempdir() -> (App, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "fc-app-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("file.txt"), b"x").unwrap();

        // App::new uses the process cwd, so point the panels at the tempdir
        // directly instead of mutating global process state.
        let theme_type = ThemeType::CatppuccinMocha;
        let app = App {
            left_panel: PanelState::new(path.clone()).unwrap(),
            right_panel: PanelState::new(path.clone()).unwrap(),
            active_pane: ActivePane::Left,
            theme_type,
            theme: theme_type.palette(),
            status_msg: None,
            should_quit: false,
        };
        (app, path)
    }

    #[test]
    fn tab_switches_panes() {
        let (mut app, dir) = app_at_tempdir();
        assert_eq!(app.active_pane, ActivePane::Left);
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.active_pane, ActivePane::Right);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn q_and_f10_quit_ctrl_c_quits() {
        for code in [KeyCode::Char('q'), KeyCode::F(10)] {
            let (mut app, dir) = app_at_tempdir();
            app.handle_key_event(key(code));
            assert!(app.should_quit);
            std::fs::remove_dir_all(dir).ok();
        }

        let (mut app, dir) = app_at_tempdir();
        app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn theme_cycle_updates_resolved_palette() {
        let (mut app, dir) = app_at_tempdir();
        let before = app.theme.name;
        app.handle_key_event(key(KeyCode::F(1)));
        assert_ne!(app.theme.name, before);
        assert_eq!(app.theme.name, app.theme_type.palette().name);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Put the cursor on the entry with this name.
    fn focus(app: &mut App, name: &str) {
        let panel = app.active_panel_mut();
        let index = panel
            .entries
            .iter()
            .position(|e| e.name == name)
            .unwrap_or_else(|| panic!("no entry named {name}"));
        panel.table_state.select(Some(index));
    }

    #[test]
    fn enter_on_a_file_requests_the_editor() {
        let (mut app, dir) = app_at_tempdir();
        focus(&mut app, "file.txt");

        let effect = app.handle_key_event(key(KeyCode::Enter));
        match effect {
            Some(Effect::OpenInEditor(path)) => assert_eq!(path, dir.join("file.txt")),
            other => panic!("expected OpenInEditor, got {other:?}"),
        }
        // The pane must not have navigated anywhere.
        assert_eq!(app.active_panel().cwd, dir);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn enter_on_a_directory_navigates_and_yields_no_effect() {
        let (mut app, dir) = app_at_tempdir();
        std::fs::create_dir(dir.join("sub")).unwrap();
        app.refresh_both_panels();
        focus(&mut app, "sub");

        assert_eq!(app.handle_key_event(key(KeyCode::Enter)), None);
        assert_eq!(app.active_panel().cwd, dir.join("sub"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn enter_on_parent_row_goes_up() {
        let (mut app, dir) = app_at_tempdir();
        std::fs::create_dir(dir.join("sub")).unwrap();
        app.refresh_both_panels();
        focus(&mut app, "sub");
        app.handle_key_event(key(KeyCode::Enter));
        assert_eq!(app.active_panel().cwd, dir.join("sub"));

        // ".." is index 0 by construction.
        app.active_panel_mut().table_state.select(Some(0));
        assert_eq!(app.handle_key_event(key(KeyCode::Enter)), None);
        assert_eq!(app.active_panel().cwd, dir);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn enter_on_a_broken_symlink_reports_instead_of_spawning() {
        let (mut app, dir) = app_at_tempdir();
        std::os::unix::fs::symlink(dir.join("missing"), dir.join("dangling")).unwrap();
        app.refresh_both_panels();
        focus(&mut app, "dangling");

        assert_eq!(app.handle_key_event(key(KeyCode::Enter)), None);
        let message = app.status_msg.as_deref().unwrap_or_default();
        assert!(message.contains("broken symlink"), "got {message:?}");
        assert_eq!(app.active_panel().cwd, dir);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn navigation_never_leaves_the_cursor_out_of_range() {
        let (mut app, dir) = app_at_tempdir();
        for _ in 0..50 {
            app.handle_key_event(key(KeyCode::Down));
        }
        let panel = app.active_panel();
        assert!(panel.cursor().unwrap() < panel.entries.len());

        for _ in 0..50 {
            app.handle_key_event(key(KeyCode::Up));
        }
        assert_eq!(app.active_panel().cursor(), Some(0));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn unbound_keys_do_nothing() {
        let (mut app, dir) = app_at_tempdir();
        for code in [
            KeyCode::Char('x'),
            KeyCode::Char('z'),
            KeyCode::F(5),
            KeyCode::Home,
        ] {
            app.handle_key_event(key(code));
        }
        assert!(!app.should_quit);
        assert_eq!(app.active_pane, ActivePane::Left);
        assert!(app.status_msg.is_none());
        std::fs::remove_dir_all(dir).ok();
    }
}
