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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::format::format_size;
use crate::fs::panel::PanelState;
use crate::tasks::{
    self, ActiveTaskHandle, ConflictPolicy, FileTask, IoOperation, TaskPhase, TaskUpdate,
};
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

/// The text buffer of an in-place rename prompt.
///
/// `cursor_pos` is a *character* index, not a byte offset: filenames are UTF-8
/// and moving a byte cursor would split multibyte characters. Every mutation
/// converts at the boundary via `byte_index`, which is the only place the two
/// representations meet.
#[derive(Debug, Clone)]
pub struct InputState {
    pub buffer: String,
    pub cursor_pos: usize,
}

impl InputState {
    /// Seed the buffer with `prefill` and put the cursor at the end, so typing
    /// appends to the existing name rather than replacing it.
    pub fn new(prefill: impl Into<String>) -> Self {
        let buffer = prefill.into();
        let cursor_pos = buffer.chars().count();
        Self { buffer, cursor_pos }
    }

    /// Insert one character at the cursor and step past it.
    pub fn insert(&mut self, c: char) {
        let at = self.byte_index();
        self.buffer.insert(at, c);
        self.cursor_pos += 1;
    }

    /// Remove the character before the cursor. A no-op at position 0.
    pub fn backspace(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let end = self.byte_index();
        let start = self.buffer[..end]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.buffer.replace_range(start..end, "");
        self.cursor_pos -= 1;
    }

    pub fn left(&mut self) {
        self.cursor_pos = self.cursor_pos.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor_pos = (self.cursor_pos + 1).min(self.buffer.chars().count());
    }

    pub fn home(&mut self) {
        self.cursor_pos = 0;
    }

    pub fn end(&mut self) {
        self.cursor_pos = self.buffer.chars().count();
    }

    /// Byte offset of the cursor, clamped to the end of the buffer.
    fn byte_index(&self) -> usize {
        self.buffer
            .char_indices()
            .nth(self.cursor_pos)
            .map(|(index, _)| index)
            .unwrap_or(self.buffer.len())
    }
}

/// What an [`ModalState::Input`] dialog will do with the typed text.
#[derive(Debug, Clone)]
pub enum InputAction {
    RenameSingle(PathBuf),
}

/// What an [`ModalState::Confirm`] dialog will do if accepted.
///
/// Phase 4 adds the copy/move/delete variants, each carrying a `FileTask`, and
/// the accept path dispatches on this instead of on a loosely-typed task field.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    /// The rename destination already exists; overwriting was acknowledged.
    RenameOverwrite { from: PathBuf, to: PathBuf },
    /// A copy, move or delete that has been fully planned and validated.
    Task { task: FileTask },
}

/// A modal dialog, or the absence of one.
///
/// Modals are exclusive: while one is open it traps every key press. That is
/// what stops a stray `q` from quitting mid-rename, or a `d` from deleting a
/// file the user was only trying to name.
#[derive(Debug)]
pub enum ModalState {
    None,
    Confirm {
        title: String,
        prompt: String,
        action: ConfirmAction,
        /// True when accepting with `s` (skip collisions) is meaningful, which
        /// is only the case for a transfer whose destination names are taken.
        allow_skip: bool,
    },
    Input {
        title: String,
        input: InputState,
        pending_op: InputAction,
    },
    /// Live progress of `App::active_task`; carries no data of its own because
    /// the handle already has it.
    Progress,
    ErrorReport {
        title: String,
        errors: Vec<String>,
        scroll_offset: u16,
    },
}

impl ModalState {
    /// True when the app is idle, i.e. panel keys are live.
    pub fn is_none(&self) -> bool {
        matches!(self, ModalState::None)
    }
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
    /// Transient one-line message (success notices, hints). Hard failures go
    /// to `modal` as an `ErrorReport` so they cannot scroll away unseen.
    pub status_msg: Option<String>,
    pub modal: ModalState,
    /// The running worker, if any. `None` means the UI owns all the state and
    /// nothing is happening in the background.
    pub active_task: Option<ActiveTaskHandle>,
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
            modal: ModalState::None,
            active_task: None,
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

        // Modals trap everything. Nothing below this line runs while one is
        // open, which is the invariant that keeps destructive keys from firing
        // at a half-typed filename.
        if !self.modal.is_none() {
            self.handle_modal_key(key);
            return None;
        }

        self.handle_panel_key(key)
    }

    /// Panel-level (non-modal) key handling.
    fn handle_panel_key(&mut self, key: KeyEvent) -> Option<Effect> {
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

            KeyCode::F(7) | KeyCode::Char('r') => {
                self.open_rename_prompt();
                None
            }

            KeyCode::F(5) | KeyCode::Char('c') => {
                self.begin_transfer(IoOperation::Copy);
                None
            }
            KeyCode::F(6) | KeyCode::Char('m') => {
                self.begin_transfer(IoOperation::Move);
                None
            }
            KeyCode::F(8) | KeyCode::Char('d') | KeyCode::Delete => {
                self.begin_delete();
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
            self.report_error("Cannot open directory", err);
        }
    }

    /// Go up one level, surfacing a failure without moving the panel.
    fn go_parent(&mut self) {
        self.clear_status();
        if let Err(err) = self.active_panel_mut().go_parent() {
            self.report_error("Cannot open directory", err);
        }
    }

    // ---------------------------------------------------------------- modals

    /// Route a key press while a modal is open.
    ///
    /// The outcome is computed inside the borrow of `self.modal` and acted on
    /// afterwards, because the actions themselves need `&mut self` (they close
    /// modals and touch the filesystem). Returning an enum instead of calling
    /// methods inline is what keeps this to one borrow.
    fn handle_modal_key(&mut self, key: KeyEvent) {
        let outcome = match &mut self.modal {
            ModalState::None => ModalOutcome::Ignore,

            ModalState::Input { input, .. } => match key.code {
                KeyCode::Esc => ModalOutcome::Cancel,
                KeyCode::Enter => ModalOutcome::SubmitInput,
                KeyCode::Backspace => {
                    input.backspace();
                    ModalOutcome::Ignore
                }
                KeyCode::Left => {
                    input.left();
                    ModalOutcome::Ignore
                }
                KeyCode::Right => {
                    input.right();
                    ModalOutcome::Ignore
                }
                KeyCode::Home => {
                    input.home();
                    ModalOutcome::Ignore
                }
                KeyCode::End => {
                    input.end();
                    ModalOutcome::Ignore
                }
                // Control chords are reserved (Ctrl+C already quit above);
                // inserting their characters would be surprising.
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.insert(c);
                    ModalOutcome::Ignore
                }
                _ => ModalOutcome::Ignore,
            },

            ModalState::Confirm { allow_skip, .. } => match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    ModalOutcome::AcceptConfirm
                }
                // Only offered when the dialog actually has collisions to skip;
                // otherwise `s` would be an undocumented no-op that looks like
                // it did something.
                KeyCode::Char('s') | KeyCode::Char('S') if *allow_skip => {
                    ModalOutcome::AcceptSkipping
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => ModalOutcome::Cancel,
                _ => ModalOutcome::Ignore,
            },

            ModalState::ErrorReport {
                errors,
                scroll_offset,
                ..
            } => {
                let max = errors.len().saturating_sub(1) as u16;
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => ModalOutcome::Cancel,
                    KeyCode::Up | KeyCode::Char('k') => {
                        *scroll_offset = scroll_offset.saturating_sub(1);
                        ModalOutcome::Ignore
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *scroll_offset = (*scroll_offset + 1).min(max);
                        ModalOutcome::Ignore
                    }
                    KeyCode::PageUp => {
                        *scroll_offset = scroll_offset.saturating_sub(10);
                        ModalOutcome::Ignore
                    }
                    KeyCode::PageDown => {
                        *scroll_offset = (*scroll_offset + 10).min(max);
                        ModalOutcome::Ignore
                    }
                    _ => ModalOutcome::Ignore,
                }
            }

            ModalState::Progress => match key.code {
                KeyCode::Esc => ModalOutcome::CancelTask,
                _ => ModalOutcome::Ignore,
            },
        };

        match outcome {
            ModalOutcome::Ignore => {}
            ModalOutcome::Cancel => {
                // Dismissing a report is not cancelling anything, so only the
                // prompts change the status line.
                let was_prompt = matches!(
                    self.modal,
                    ModalState::Input { .. } | ModalState::Confirm { .. }
                );
                self.modal = ModalState::None;
                if was_prompt {
                    self.status_msg = Some("Cancelled.".to_string());
                }
            }
            ModalOutcome::SubmitInput => self.submit_input(),
            ModalOutcome::AcceptConfirm => self.accept_confirm(false),
            ModalOutcome::AcceptSkipping => self.accept_confirm(true),
            ModalOutcome::CancelTask => {
                // The modal stays open until the worker acknowledges: closing it
                // immediately would let the user start an overlapping task on
                // files that are still being written.
                if let Some(task) = &mut self.active_task {
                    task.request_cancel();
                    task.phase = TaskPhase::Cancelling;
                } else {
                    // Defensive: a progress dialog with no worker behind it would
                    // otherwise trap every key forever.
                    self.modal = ModalState::None;
                }
            }
        }
    }

    /// Open the rename prompt pre-filled with the cursor entry's name.
    fn open_rename_prompt(&mut self) {
        let Some(entry) = self.active_panel().selected_entry() else {
            return;
        };
        if entry.is_parent_nav {
            self.status_msg = Some("Cannot rename \"..\"".to_string());
            return;
        }

        let path = entry.path.clone();
        let name = entry.name.clone();
        self.clear_status();
        self.modal = ModalState::Input {
            title: format!("Rename  {}", path.display()),
            input: InputState::new(name),
            pending_op: InputAction::RenameSingle(path),
        };
    }

    /// Act on a submitted rename prompt.
    fn submit_input(&mut self) {
        // Take the modal out so `self` is free for the filesystem work below.
        // If the variant is not `Input` (impossible via the routing above) the
        // modal stays closed rather than half-open.
        let ModalState::Input {
            input, pending_op, ..
        } = std::mem::replace(&mut self.modal, ModalState::None)
        else {
            return;
        };

        let InputAction::RenameSingle(from) = pending_op;
        let to = match valid_rename_target(&from, &input.buffer) {
            Ok(to) => to,
            Err(message) => {
                self.show_error("Invalid name", vec![message]);
                return;
            }
        };

        if to == from {
            self.status_msg = Some("Name unchanged.".to_string());
            return;
        }

        // `symlink_metadata` rather than `exists`: a dangling symlink still
        // occupies the name, and `exists` would report false and let rename
        // silently replace it.
        if fs::symlink_metadata(&to).is_ok() {
            self.modal = ModalState::Confirm {
                title: "Overwrite?".to_string(),
                prompt: format!(
                    "{} already exists.\nOverwriting replaces it permanently.",
                    to.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| to.display().to_string())
                ),
                action: ConfirmAction::RenameOverwrite { from, to },
                allow_skip: false,
            };
            return;
        }

        self.perform_rename(from, to);
    }

    /// Accept the currently open confirm dialog.
    ///
    /// `skip_existing` is the "do not overwrite" branch of a conflict dialog; it
    /// only has an effect on a transfer.
    fn accept_confirm(&mut self, skip_existing: bool) {
        let ModalState::Confirm { action, .. } =
            std::mem::replace(&mut self.modal, ModalState::None)
        else {
            return;
        };

        match action {
            ConfirmAction::RenameOverwrite { from, to } => self.perform_rename(from, to),
            ConfirmAction::Task { mut task } => {
                if skip_existing {
                    task.policy = ConflictPolicy::Skip;
                }
                self.start_task(task);
            }
        }
    }

    // -------------------------------------------------------------- transfers

    /// Plan and confirm a copy or move into the inactive pane.
    ///
    /// Everything that can be decided without user input is decided here:
    /// nested sources are collapsed, impossible targets are rejected before any
    /// dialog appears, and existing destination names are collected so the
    /// confirmation can name them instead of silently clobbering.
    fn begin_transfer(&mut self, op: IoOperation) {
        if self.active_task.is_some() {
            self.status_msg = Some("A task is already running.".to_string());
            return;
        }

        let sources = self.active_panel().operated_paths();
        if sources.is_empty() {
            self.status_msg = Some("Nothing selected.".to_string());
            return;
        }

        let destination = self.inactive_panel().cwd.clone();
        if destination == self.active_panel().cwd {
            self.status_msg = Some("Both panes show the same directory.".to_string());
            return;
        }

        let (task, conflicts) = match plan_transfer(op, sources, destination) {
            Ok(plan) => plan,
            Err(problems) => {
                self.show_error("Cannot start", problems);
                return;
            }
        };

        self.clear_status();
        let count = task.sources.len();
        let target = task
            .destination
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();

        if conflicts.is_empty() {
            self.modal = ModalState::Confirm {
                title: format!("{} {}", op.verb(), item_count(count)),
                prompt: format!("Destination:\n{target}"),
                action: ConfirmAction::Task { task },
                allow_skip: false,
            };
        } else {
            // Name the collisions rather than counting them: the user is being
            // asked to destroy data, and "2 conflicts" does not say which two.
            let listed: Vec<String> = conflicts
                .iter()
                .take(3)
                .map(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string())
                })
                .collect();
            let more = conflicts.len().saturating_sub(listed.len());
            let mut names = listed.join(", ");
            if more > 0 {
                names.push_str(&format!(" and {more} more"));
            }

            self.modal = ModalState::Confirm {
                title: format!(
                    "{} {} — {} already present",
                    op.verb(),
                    item_count(count),
                    item_count(conflicts.len())
                ),
                prompt: format!(
                    "Destination:\n{target}\n\nAlready present:\n{names}\n\nOverwrite replaces them; skip leaves them untouched."
                ),
                action: ConfirmAction::Task { task },
                allow_skip: true,
            };
        }
    }

    /// Plan and confirm a delete of the selection (or the cursor row).
    fn begin_delete(&mut self) {
        if self.active_task.is_some() {
            self.status_msg = Some("A task is already running.".to_string());
            return;
        }

        let sources = dedupe_nested(self.active_panel().operated_paths());
        if sources.is_empty() {
            self.status_msg = Some("Nothing selected.".to_string());
            return;
        }

        let count = sources.len();
        self.clear_status();
        self.modal = ModalState::Confirm {
            title: format!("Delete {}", item_count(count)),
            prompt: "These entries are removed permanently.".to_string(),
            action: ConfirmAction::Task {
                task: FileTask {
                    op: IoOperation::Delete,
                    sources,
                    destination: None,
                    policy: ConflictPolicy::Overwrite,
                },
            },
            allow_skip: false,
        };
    }

    /// Hand a confirmed task to a worker and switch the UI into progress mode.
    fn start_task(&mut self, task: FileTask) {
        if self.active_task.is_some() {
            self.modal = ModalState::None;
            self.status_msg = Some("A task is already running.".to_string());
            return;
        }

        self.status_msg = None;
        self.modal = ModalState::Progress;
        self.active_task = Some(tasks::spawn_worker(task));
    }

    /// Consume up to `max_updates` worker messages.
    ///
    /// Called once per frame from the event loop. The message cap is what stops
    /// a chatty worker from starving rendering: whatever is left waits for the
    /// next frame 16 ms later.
    ///
    /// The handle is temporarily taken out of `self` so that `self` stays free
    /// for the state changes the updates cause (refreshing panes, opening the
    /// error report) — the same double-borrow problem the specification's
    /// original loop had.
    pub fn pump_worker(&mut self, max_updates: usize) {
        let Some(mut task) = self.active_task.take() else {
            return;
        };

        let mut terminal: Option<TaskUpdate> = None;

        for _ in 0..max_updates {
            match task.rx.try_recv() {
                Ok(update) => {
                    let done = apply_task_update(&mut task, &update);
                    if done {
                        terminal = Some(update);
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The worker died without a final message (a panic, or the
                    // thread was killed). Do not leave the UI stuck on a
                    // progress bar that will never move.
                    terminal = Some(TaskUpdate::Aborted {
                        reason: "worker stopped unexpectedly".to_string(),
                        items_done: task.items_done,
                        bytes_done: task.bytes_done,
                    });
                    break;
                }
            }
        }

        match terminal {
            Some(update) => self.finish_task(update, &task),
            None => self.active_task = Some(task),
        }
    }

    /// Close out a finished task: resynchronise the panes, drop the selection,
    /// and report the outcome.
    fn finish_task(&mut self, update: TaskUpdate, task: &ActiveTaskHandle) {
        self.active_task = None;
        self.refresh_both_panels();

        // The operands are gone or moved; keeping the selection would let the
        // next operation act on paths that no longer exist.
        self.left_panel.selected.clear();
        self.right_panel.selected.clear();

        match update {
            TaskUpdate::Completed {
                skipped_errors,
                skipped,
            } => {
                let mut problems: Vec<String> = skipped_errors
                    .iter()
                    .map(|(path, error)| format!("{}: {error}", path.display()))
                    .collect();
                problems.extend(
                    skipped
                        .iter()
                        .map(|path| format!("{}: skipped (destination exists)", path.display())),
                );

                if problems.is_empty() {
                    self.modal = ModalState::None;
                    self.status_msg = Some(format!(
                        "{} complete: {}, {}",
                        task.op.verb(),
                        item_count(task.items_done),
                        format_size(task.bytes_done)
                    ));
                } else {
                    self.show_error("Finished with problems", problems);
                }
            }
            TaskUpdate::Aborted {
                reason,
                items_done,
                bytes_done,
            } => {
                self.modal = ModalState::None;
                self.status_msg = Some(format!(
                    "{reason} after {}, {}",
                    item_count(items_done),
                    format_size(bytes_done)
                ));
            }
            _ => {}
        }
    }

    /// Rename on disk, then resynchronise the panes with reality.
    fn perform_rename(&mut self, from: PathBuf, to: PathBuf) {
        match fs::rename(&from, &to) {
            Ok(()) => {
                self.refresh_both_panels();
                self.active_panel_mut().select_path(&to);
                let name = to
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| to.display().to_string());
                self.status_msg = Some(format!("Renamed to {name}"));
            }
            Err(err) => {
                self.show_error(
                    "Rename failed",
                    vec![format!("{} -> {}: {err}", from.display(), to.display())],
                );
            }
        }
    }

    /// Surface a filesystem failure as an error report the user has to dismiss.
    fn report_error(&mut self, title: &str, err: anyhow::Error) {
        self.show_error(title, vec![format!("{err:#}")]);
    }

    /// Open (or replace) the error report modal.
    fn show_error(&mut self, title: &str, errors: Vec<String>) {
        self.status_msg = None;
        self.modal = ModalState::ErrorReport {
            title: title.to_string(),
            errors,
            scroll_offset: 0,
        };
    }
}

/// What a key press inside a modal asked for, resolved after the borrow ends.
enum ModalOutcome {
    Ignore,
    Cancel,
    SubmitInput,
    AcceptConfirm,
    /// Accept a transfer confirmation while skipping existing destinations.
    AcceptSkipping,
    /// Ask the running worker to stop; the modal stays until it answers.
    CancelTask,
}

/// Fold one worker message into the handle the UI renders from.
///
/// Returns true when the message is terminal, i.e. the worker will not speak
/// again.
fn apply_task_update(task: &mut ActiveTaskHandle, update: &TaskUpdate) -> bool {
    match update {
        TaskUpdate::Started {
            total_items,
            total_bytes,
        } => {
            task.total_items = *total_items;
            task.total_bytes = *total_bytes;
            // A cancel can arrive while the pre-flight scan is still running;
            // do not let the scan's completion undo the "cancelling" state.
            if task.phase == TaskPhase::Scanning {
                task.phase = TaskPhase::Running;
            }
        }
        TaskUpdate::Progress {
            current_file,
            items_processed,
            bytes_processed,
        } => {
            task.current_file = current_file.clone();
            task.items_done = *items_processed;
            task.bytes_done = *bytes_processed;
        }
        TaskUpdate::NonFatalError { path, error } => {
            task.error_count += 1;
            task.last_error = Some(format!("{}: {error}", path.display()));
        }
        TaskUpdate::Completed { skipped, .. } => task.skipped_count = skipped.len(),
        TaskUpdate::Aborted { .. } => {}
    }

    update.is_terminal()
}

/// Turn a selection plus a destination into a validated task.
///
/// Returns the conflicts separately from the task: the caller decides how to
/// ask about them, and the task itself only carries the policy that the answer
/// produces.
fn plan_transfer(
    op: IoOperation,
    sources: Vec<PathBuf>,
    destination: PathBuf,
) -> Result<(FileTask, Vec<PathBuf>), Vec<String>> {
    let sources = dedupe_nested(sources);
    let mut problems = Vec::new();
    let mut conflicts = Vec::new();

    for source in &sources {
        let Some(name) = source.file_name() else {
            problems.push(format!("{}: has no file name", source.display()));
            continue;
        };

        let target = destination.join(name);

        if target == *source {
            problems.push(format!(
                "{}: source and destination are the same path",
                source.display()
            ));
            continue;
        }

        // Component-wise prefix test, so "/a/b" is caught as being inside
        // "/a/b" while "/a/bc" is not.
        if target.starts_with(source) {
            problems.push(format!(
                "cannot {} {} into itself",
                op.verb().to_lowercase(),
                source.display()
            ));
            continue;
        }

        // symlink_metadata, not exists(): a dangling symlink occupies the name.
        if fs::symlink_metadata(&target).is_ok() {
            conflicts.push(target);
        }
    }

    if !problems.is_empty() {
        return Err(problems);
    }

    Ok((
        FileTask {
            op,
            sources,
            destination: Some(destination),
            policy: ConflictPolicy::Overwrite,
        },
        conflicts,
    ))
}

/// Collapse a selection that contains both a directory and its own descendants.
///
/// Selecting `src` and `src/sub/file` would otherwise copy the file twice (and
/// delete it once and then fail on it), and reporting "1 item" while doing two
/// units of work is the kind of thing that makes a progress bar lie.
fn dedupe_nested(mut sources: Vec<PathBuf>) -> Vec<PathBuf> {
    // Sort so ancestors are seen before their children; then anything already
    // covered by an accepted ancestor is dropped.
    sources.sort();
    let mut kept: Vec<PathBuf> = Vec::new();

    for path in sources {
        if kept.iter().any(|ancestor| path.starts_with(ancestor)) {
            continue;
        }
        kept.push(path);
    }

    kept
}

/// "1 item" / "3 items", so no dialog says "1 items".
fn item_count(count: usize) -> String {
    if count == 1 {
        "1 item".to_string()
    } else {
        format!("{count} items")
    }
}

/// Validate a typed name and resolve it against the source file's directory.
///
/// Returns an error message fit for display rather than an `io::Error`, because
/// the failure is the user's input, not the filesystem's. `\0` and `/` are
/// rejected rather than passed through: both produce `EINVAL`/`ENOENT` from the
/// kernel at best, and `/` would quietly turn "rename" into "move elsewhere".
fn valid_rename_target(from: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("A name is required.".to_string());
    }
    if raw == "." || raw == ".." {
        return Err(format!(
            "\"{raw}\" is reserved and cannot be used as a name."
        ));
    }
    if raw.contains('/') {
        return Err("A name cannot contain '/'.".to_string());
    }
    if raw.contains('\0') {
        return Err("A name cannot contain a NUL byte.".to_string());
    }

    let parent = from.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent.join(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Unique per call: a nanosecond timestamp alone collides when two tests
    /// start in the same clock tick, and both of them then delete each other's
    /// directory halfway through.
    fn app_at_tempdir() -> (App, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "fc-app-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
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
            modal: ModalState::None,
            active_task: None,
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
            KeyCode::F(4),
            KeyCode::Home,
        ] {
            app.handle_key_event(key(code));
        }
        assert!(!app.should_quit);
        assert_eq!(app.active_pane, ActivePane::Left);
        assert!(app.status_msg.is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    // ---------------------------------------------------------- modals & rename

    /// Read the rename buffer, or panic with the modal that is actually open.
    fn buffer(app: &App) -> &str {
        match &app.modal {
            ModalState::Input { input, .. } => &input.buffer,
            other => panic!("expected an Input modal, got {other:?}"),
        }
    }

    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key_event(key(KeyCode::Char(c)));
        }
    }

    fn backspace_n(app: &mut App, count: usize) {
        for _ in 0..count {
            app.handle_key_event(key(KeyCode::Backspace));
        }
    }

    #[test]
    fn f7_opens_a_prompt_prefilled_with_the_current_name() {
        let (mut app, dir) = app_at_tempdir();
        focus(&mut app, "file.txt");

        app.handle_key_event(key(KeyCode::F(7)));
        assert_eq!(buffer(&app), "file.txt");

        // Cursor sits at the end, so typing appends.
        type_str(&mut app, "_v2");
        assert_eq!(buffer(&app), "file.txt_v2");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn enter_renames_on_disk_and_moves_the_cursor() {
        let (mut app, dir) = app_at_tempdir();
        focus(&mut app, "file.txt");

        app.handle_key_event(key(KeyCode::Char('r')));
        type_str(&mut app, "_v2");
        app.handle_key_event(key(KeyCode::Enter));

        assert!(app.modal.is_none(), "modal should close after a rename");
        assert!(!dir.join("file.txt").exists());
        assert!(dir.join("file.txt_v2").exists());

        assert_eq!(
            app.active_panel().selected_entry().map(|e| e.name.as_str()),
            Some("file.txt_v2")
        );
        assert!(app.status_msg.as_deref().unwrap_or("").contains("Renamed"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn esc_discards_the_rename() {
        let (mut app, dir) = app_at_tempdir();
        focus(&mut app, "file.txt");

        app.handle_key_event(key(KeyCode::F(7)));
        type_str(&mut app, "_discarded");
        app.handle_key_event(key(KeyCode::Esc));

        assert!(app.modal.is_none());
        assert!(dir.join("file.txt").exists());
        assert!(!dir.join("file.txt_discarded").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn renaming_onto_an_existing_name_requires_confirmation() {
        let (mut app, dir) = app_at_tempdir();
        std::fs::write(dir.join("other.txt"), b"original").unwrap();
        app.refresh_both_panels();
        focus(&mut app, "file.txt");

        app.handle_key_event(key(KeyCode::F(7)));
        // Clear "file.txt" (8 chars) and type the taken name.
        backspace_n(&mut app, "file.txt".chars().count());
        type_str(&mut app, "other.txt");
        app.handle_key_event(key(KeyCode::Enter));

        match &app.modal {
            ModalState::Confirm { prompt, .. } => {
                assert!(prompt.contains("other.txt"), "prompt was {prompt:?}");
            }
            other => panic!("expected a Confirm modal, got {other:?}"),
        }
        // Nothing has happened yet: confirmation is not a formality.
        assert!(dir.join("file.txt").exists());
        assert_eq!(std::fs::read(dir.join("other.txt")).unwrap(), b"original");

        app.handle_key_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(!dir.join("file.txt").exists());
        // app_at_tempdir writes file.txt with exactly the byte b"x".
        assert_eq!(std::fs::read(dir.join("other.txt")).unwrap(), b"x");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cancelling_the_overwrite_confirmation_leaves_both_files_alone() {
        let (mut app, dir) = app_at_tempdir();
        std::fs::write(dir.join("other.txt"), b"original").unwrap();
        app.refresh_both_panels();
        focus(&mut app, "file.txt");

        app.handle_key_event(key(KeyCode::F(7)));
        backspace_n(&mut app, "file.txt".chars().count());
        type_str(&mut app, "other.txt");
        app.handle_key_event(key(KeyCode::Enter));
        app.handle_key_event(key(KeyCode::Esc));

        assert!(app.modal.is_none());
        assert!(dir.join("file.txt").exists());
        assert_eq!(std::fs::read(dir.join("other.txt")).unwrap(), b"original");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn invalid_names_are_rejected_by_the_pure_validator() {
        let from = Path::new("/tmp/x/file.txt");

        assert!(valid_rename_target(from, "").is_err());
        assert!(valid_rename_target(from, ".").is_err());
        assert!(valid_rename_target(from, "..").is_err());
        assert!(valid_rename_target(from, "a/b").is_err());
        assert!(valid_rename_target(from, "a\0b").is_err());

        assert_eq!(
            valid_rename_target(from, "new name.txt").unwrap(),
            Path::new("/tmp/x/new name.txt")
        );
        // A name that merely contains dots is fine.
        assert!(valid_rename_target(from, "...").is_ok());
        assert!(valid_rename_target(from, ".hidden").is_ok());
    }

    #[test]
    fn invalid_rename_opens_an_error_report_instead_of_touching_disk() {
        let (mut app, dir) = app_at_tempdir();
        focus(&mut app, "file.txt");

        app.handle_key_event(key(KeyCode::F(7)));
        backspace_n(&mut app, 8);
        type_str(&mut app, "a/b");
        app.handle_key_event(key(KeyCode::Enter));

        match &app.modal {
            ModalState::ErrorReport { title, errors, .. } => {
                assert_eq!(title, "Invalid name");
                assert!(errors[0].contains("'/'"), "errors were {errors:?}");
            }
            other => panic!("expected an ErrorReport, got {other:?}"),
        }
        assert!(dir.join("file.txt").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn keys_are_trapped_while_a_modal_is_open() {
        let (mut app, dir) = app_at_tempdir();
        focus(&mut app, "file.txt");
        app.handle_key_event(key(KeyCode::F(7)));

        // 'q' would quit from panel mode; here it must be text.
        app.handle_key_event(key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        assert!(buffer(&app).ends_with('q'));

        // Tab must not switch panes either.
        app.handle_key_event(key(KeyCode::Tab));
        assert_eq!(app.active_pane, ActivePane::Left);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn multibyte_names_survive_editing() {
        let (mut app, dir) = app_at_tempdir();
        std::fs::write(dir.join("café.txt"), b"x").unwrap();
        app.refresh_both_panels();
        focus(&mut app, "café.txt");

        app.handle_key_event(key(KeyCode::F(7)));
        backspace_n(&mut app, "café.txt".chars().count());
        type_str(&mut app, "🦀.txt");
        app.handle_key_event(key(KeyCode::Enter));

        assert!(dir.join("🦀.txt").exists(), "multibyte rename failed");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn input_state_edits_by_character_not_byte() {
        let mut input = InputState::new("héllo");
        assert_eq!(input.cursor_pos, 5, "cursor counted in chars");

        // Step over 'h' and 'é', then delete backwards: a byte-indexed
        // implementation would cut the middle out of 'é' and produce invalid
        // UTF-8 here.
        input.home();
        input.right();
        input.right();
        input.backspace();
        assert_eq!(input.buffer, "hllo");
        assert_eq!(input.cursor_pos, 1);

        input.insert('é');
        assert_eq!(input.buffer, "héllo");

        input.end();
        input.right(); // clamped at the end
        assert_eq!(input.cursor_pos, 5);
    }

    #[test]
    fn failed_directory_entry_opens_an_error_report() {
        let (mut app, dir) = app_at_tempdir();
        std::fs::create_dir(dir.join("vanishing")).unwrap();
        app.refresh_both_panels();
        focus(&mut app, "vanishing");

        // The directory is gone between listing and entry.
        std::fs::remove_dir(dir.join("vanishing")).unwrap();
        app.handle_key_event(key(KeyCode::Enter));

        match &app.modal {
            ModalState::ErrorReport { title, .. } => assert_eq!(title, "Cannot open directory"),
            other => panic!("expected an ErrorReport, got {other:?}"),
        }
        // Esc dismisses and the pane is still usable.
        app.handle_key_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(app.active_panel().cwd, dir);
        std::fs::remove_dir_all(dir).ok();
    }

    // ---------------------------------------------------------------- tasks

    /// Two panes in sibling directories, plus the directory that holds both.
    fn app_two_panes() -> (App, PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let base = std::env::temp_dir().join(format!(
            "fc-xfer-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let left = base.join("left");
        let right = base.join("right");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::create_dir_all(&right).unwrap();

        let theme_type = ThemeType::CatppuccinMocha;
        let app = App {
            left_panel: PanelState::new(left).unwrap(),
            right_panel: PanelState::new(right).unwrap(),
            active_pane: ActivePane::Left,
            theme_type,
            theme: theme_type.palette(),
            status_msg: None,
            modal: ModalState::None,
            active_task: None,
            should_quit: false,
        };
        (app, base)
    }

    /// Spin the pump until the worker reports a terminal message. Bounded so a
    /// hung worker fails the test instead of hanging the suite.
    fn wait_for_task(app: &mut App) {
        for _ in 0..1500 {
            app.pump_worker(100);
            if app.active_task.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("worker did not finish within three seconds");
    }

    #[test]
    fn copy_confirms_first_then_writes_and_refreshes() {
        let (mut app, base) = app_two_panes();
        let source = base.join("left").join("alpha.txt");
        let dest = base.join("right").join("alpha.txt");
        std::fs::write(&source, b"payload").unwrap();
        app.left_panel.refresh().unwrap();
        app.left_panel.select_path(&source);

        app.handle_key_event(key(KeyCode::F(5)));

        assert!(
            matches!(app.modal, ModalState::Confirm { .. }),
            "copy must ask before touching the disk"
        );
        assert!(!dest.exists(), "nothing may be copied before confirmation");

        app.handle_key_event(key(KeyCode::Enter));
        assert!(
            app.active_task.is_some(),
            "confirming should start the worker"
        );
        wait_for_task(&mut app);

        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        assert!(
            matches!(app.modal, ModalState::None),
            "dialog closes on success"
        );
        assert!(
            app.status_msg.as_deref().unwrap_or("").starts_with("Copy"),
            "status should summarise the operation, got {:?}",
            app.status_msg
        );
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_conflict_offers_skip_and_skip_leaves_the_destination_alone() {
        let (mut app, base) = app_two_panes();
        let source = base.join("left").join("alpha.txt");
        let dest = base.join("right").join("alpha.txt");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();
        app.left_panel.refresh().unwrap();
        app.left_panel.select_path(&source);

        app.handle_key_event(key(KeyCode::F(5)));
        match &app.modal {
            ModalState::Confirm { allow_skip, .. } => {
                assert!(allow_skip, "a clash must offer skip as well as overwrite");
            }
            other => panic!("expected a conflict confirmation, got {other:?}"),
        }

        // 's' would normally cycle the sort order; while a modal is open it must
        // only mean skip.
        app.handle_key_event(key(KeyCode::Char('s')));
        wait_for_task(&mut app);

        assert_eq!(std::fs::read(&dest).unwrap(), b"old", "skip must not write");
        assert!(
            matches!(app.modal, ModalState::ErrorReport { .. }),
            "a skip is reported, not silently swallowed"
        );
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_conflict_can_be_overwritten() {
        let (mut app, base) = app_two_panes();
        let source = base.join("left").join("alpha.txt");
        let dest = base.join("right").join("alpha.txt");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();
        app.left_panel.refresh().unwrap();
        app.left_panel.select_path(&source);

        app.handle_key_event(key(KeyCode::F(5)));
        app.handle_key_event(key(KeyCode::Enter));
        wait_for_task(&mut app);

        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn move_removes_the_source_once_the_copy_is_safe() {
        let (mut app, base) = app_two_panes();
        let source = base.join("left").join("beta.txt");
        let dest = base.join("right").join("beta.txt");
        std::fs::write(&source, b"carry").unwrap();
        app.left_panel.refresh().unwrap();
        app.left_panel.select_path(&source);

        app.handle_key_event(key(KeyCode::F(6)));
        assert!(matches!(app.modal, ModalState::Confirm { .. }));
        app.handle_key_event(key(KeyCode::Enter));
        wait_for_task(&mut app);

        assert_eq!(std::fs::read(&dest).unwrap(), b"carry");
        assert!(!source.exists(), "the source is gone after a move");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn delete_is_cancellable_and_permanent_once_confirmed() {
        let (mut app, base) = app_two_panes();
        let victim = base.join("left").join("gamma.txt");
        std::fs::write(&victim, b"bye").unwrap();
        app.left_panel.refresh().unwrap();
        app.left_panel.select_path(&victim);

        app.handle_key_event(key(KeyCode::F(8)));
        assert!(matches!(app.modal, ModalState::Confirm { .. }));
        app.handle_key_event(key(KeyCode::Esc));
        assert!(matches!(app.modal, ModalState::None));
        assert!(victim.exists(), "escaping must not delete anything");

        app.handle_key_event(key(KeyCode::F(8)));
        app.handle_key_event(key(KeyCode::Enter));
        wait_for_task(&mut app);

        assert!(!victim.exists());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_running_task_blocks_a_second_one() {
        let (mut app, base) = app_two_panes();
        let source = base.join("left").join("alpha.txt");
        std::fs::write(&source, b"payload").unwrap();
        app.left_panel.refresh().unwrap();
        app.left_panel.select_path(&source);

        app.handle_key_event(key(KeyCode::F(5)));
        app.handle_key_event(key(KeyCode::Enter));

        // While a task runs the progress dialog traps every key, so a second F5
        // cannot even reach the planner.
        app.handle_key_event(key(KeyCode::F(5)));
        assert!(matches!(app.modal, ModalState::Progress));
        assert!(app.status_msg.is_none());

        // And if the dialog is out of the way, the guard still refuses.
        app.modal = ModalState::None;
        app.begin_transfer(IoOperation::Copy);
        assert!(
            app.status_msg
                .as_deref()
                .unwrap_or("")
                .contains("already running"),
            "got {:?}",
            app.status_msg
        );
        wait_for_task(&mut app);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_large_copy_reports_live_progress_not_a_scan() {
        let (mut app, base) = app_two_panes();
        let source = base.join("left").join("big.bin");
        let dest = base.join("right").join("big.bin");
        let file = std::fs::File::create(&source).unwrap();
        file.set_len(96 * 1024 * 1024).unwrap();
        drop(file);

        app.left_panel.refresh().unwrap();
        app.left_panel.select_path(&source);
        app.handle_key_event(key(KeyCode::F(5)));
        app.handle_key_event(key(KeyCode::Enter));

        // The scan must resolve into a running task within a frame or two:
        // otherwise the dialog sits on "Measuring the work" for the whole copy.
        let mut running = false;
        for _ in 0..500 {
            app.pump_worker(100);
            match &app.active_task {
                Some(task) if task.phase == TaskPhase::Running => {
                    assert_eq!(task.total_bytes, 96 * 1024 * 1024);
                    assert_eq!(task.total_items, 1);
                    running = true;
                    break;
                }
                Some(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
                None => break,
            }
        }
        assert!(running, "task never left the scanning phase");
        wait_for_task(&mut app);
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 96 * 1024 * 1024);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn nested_selections_collapse_to_their_ancestor() {
        let base = PathBuf::from("/tmp/fc-dedupe");
        let picked = dedupe_nested(vec![
            base.join("a/b/c"),
            base.join("a"),
            base.join("d"),
            base.join("a/b"),
        ]);
        assert_eq!(picked, vec![base.join("a"), base.join("d")]);
    }

    #[test]
    fn copying_a_directory_into_itself_is_refused_early() {
        let base = PathBuf::from("/tmp/fc-self");
        let dir = base.join("sub");
        let plan = plan_transfer(IoOperation::Copy, vec![dir.clone()], dir.clone());
        let problems = plan.expect_err("a directory cannot be copied into itself");
        assert!(
            problems[0].contains("itself"),
            "the message should say why: {problems:?}"
        );

        // The same path as source and destination is equally meaningless.
        let plan = plan_transfer(IoOperation::Copy, vec![base.join("file.txt")], base.clone());
        let problems = plan.expect_err("source and destination are identical");
        assert!(problems[0].contains("same path"), "{problems:?}");
    }
}
