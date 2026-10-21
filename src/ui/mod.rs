//! Frame composition: two panes, a status line and an F-key bar.
//!
//! Layout is three vertical bands — body (grows), status (1 row), keys (1 row) —
//! and the body splits 50/50 horizontally. Percentages are used for the split so
//! that an odd column count cannot leave a one-column gap between the panes.
//!
//! This module only decides *where* things go. What a pane looks like lives in
//! `panel_view`, and overlays live in `modal` (Phase 3), so the renderer stays
//! readable as features are added.

pub mod modal;
pub mod panel_view;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{ActivePane, App};
use crate::theme::Theme;

/// Keys advertised in the bottom bar. Grows with each phase so the bar never
/// promises an action that is not implemented yet.
/// Deliberately short labels: the bar is one row and the leftmost hints are the
/// ones a new user needs. Selection keys are omitted because Space is discoverable
/// from the spec and the row has a hard width budget on an 80-column terminal.
const KEY_HINTS: &[(&str, &str)] = &[
    ("F1", "Theme"),
    ("F2", "Sort"),
    ("F5", "Copy"),
    ("F6", "Move"),
    ("F7", "Ren"),
    ("F8", "Del"),
    ("Tab", "Pane"),
    ("Enter", "Open"),
    ("F10", "Quit"),
];

/// Draw one complete frame.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let theme = app.theme;

    let bands = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(bands[0]);

    panel_view::render(
        frame,
        &app.left_panel,
        panes[0],
        app.active_pane == ActivePane::Left,
        &theme,
    );
    panel_view::render(
        frame,
        &app.right_panel,
        panes[1],
        app.active_pane == ActivePane::Right,
        &theme,
    );

    render_status(frame, app, bands[1]);
    render_key_bar(frame, &theme, bands[2]);

    // Modals paint last so they cover the panes rather than being clipped by
    // them; they also `Clear` their own rectangle.
    modal::render(frame, app);
}

/// Status line: what the app is doing, plus a per-pane tally.
fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    let panel = app.active_panel();

    let message = match &app.status_msg {
        Some(msg) => Span::styled(msg.clone(), Style::default().fg(theme.error_fg)),
        None => Span::styled("Ready.", Style::default().fg(theme.border_inactive)),
    };

    let tally = format!(
        "{} selected ({}) / {} items ",
        panel.selected.len(),
        crate::format::format_size(panel.selected_bytes()),
        panel.entries.iter().filter(|e| !e.is_parent_nav).count()
    );

    let line = Line::from(vec![
        Span::styled(
            format!("[{}] ", theme.name),
            Style::default().fg(theme.dir_fg),
        ),
        message,
    ]);

    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme.bg)),
        area,
    );

    // Right-aligned tally. Drawn separately so a long error message on the left
    // cannot push it off screen.
    let right = Paragraph::new(Line::from(Span::styled(
        tally,
        Style::default().fg(theme.border_inactive),
    )))
    .alignment(ratatui::layout::Alignment::Right)
    .style(Style::default().bg(theme.bg));

    frame.render_widget(right, area);
}

/// The F-key hint bar, drawn as one coloured strip.
fn render_key_bar(frame: &mut Frame, theme: &Theme, area: Rect) {
    let mut spans = Vec::new();
    let mut used: usize = 0;

    for (key, label) in KEY_HINTS {
        let width = key.chars().count() + label.chars().count() + 3;
        // Stop before a hint would be cut in half by the frame edge. Clipping
        // a key name ("F1" shown as "F") is worse than omitting the hint.
        if used + width > area.width as usize {
            break;
        }
        used += width;

        spans.push(Span::styled(
            format!(" {key} "),
            Style::default().bg(theme.fkey_bg).fg(theme.fkey_fg),
        ));
        spans.push(Span::styled(
            format!("{label} "),
            Style::default().bg(theme.fkey_bg).fg(theme.fg),
        ));
    }

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.fkey_bg));
    frame.render_widget(bar, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::panel::PanelState;
    use crate::theme::ThemeType;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    /// Render one frame into an 80x24 offscreen buffer and return its text.
    ///
    /// This is the cheapest honest check that layout, which is pure geometry,
    /// actually puts pixels where we think: no terminal required.
    fn render_to_string(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render(frame, app))
            .expect("draw frame");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn app_at(dir: PathBuf) -> App {
        let theme_type = ThemeType::CatppuccinMocha;
        App {
            left_panel: PanelState::new(dir.clone()).expect("left panel"),
            right_panel: PanelState::new(dir).expect("right panel"),
            active_pane: ActivePane::Left,
            theme_type,
            theme: theme_type.palette(),
            status_msg: None,
            modal: crate::app::ModalState::None,
            active_task: None,
            should_quit: false,
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "fc-ui-test-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn frame_contains_both_panes_status_and_key_bar() {
        let dir = tempdir("frame");
        std::fs::write(dir.join("visible-file.txt"), b"hello").unwrap();
        let app = app_at(dir.clone());

        let text = render_to_string(&app, 100, 24);

        assert!(text.contains("visible-file.txt"), "file row missing");
        // Two panes, so the status/footer text appears twice for the cwd.
        assert_eq!(text.matches("Catppuccin Mocha").count(), 1);
        assert!(text.contains("F10"), "key bar missing");
        assert!(text.contains("Quit"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn long_listings_scroll_the_cursor_into_view() {
        let dir = tempdir("scroll");
        for i in 0..60 {
            std::fs::write(dir.join(format!("file-{i:02}.txt")), b"x").unwrap();
        }
        let mut app = app_at(dir.clone());
        // Give the right pane an empty directory: otherwise it mirrors the same
        // listing at cursor 0 and the "top scrolled away" check below could
        // never pass, since it would find file-00.txt in the other pane.
        app.right_panel = PanelState::new(tempdir("scroll-empty")).unwrap();
        // Walk to the very last row (".." + 60 files means index 60).
        while app.active_panel().cursor().unwrap() < app.active_panel().entries.len() - 1 {
            app.active_panel_mut().move_down();
        }

        let text = render_to_string(&app, 100, 24);
        assert!(
            text.contains("file-59.txt"),
            "cursor row not scrolled into view"
        );
        assert!(
            !text.contains("file-00.txt"),
            "the top of the listing should have scrolled away"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn empty_pane_shows_a_hint_instead_of_blank_space() {
        let dir = tempdir("empty");
        let app = app_at(dir.clone());
        let text = render_to_string(&app, 100, 24);
        assert!(text.contains("empty directory"), "hint missing");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn selected_rows_are_marked_with_an_asterisk() {
        let dir = tempdir("marker");
        std::fs::write(dir.join("picked.txt"), b"x").unwrap();
        let mut app = app_at(dir.clone());
        // Cursor starts on "..", which cannot be selected, so move first.
        app.active_panel_mut().move_down();
        app.handle_key_event(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(' '),
            crossterm::event::KeyModifiers::NONE,
        ));

        let text = render_to_string(&app, 100, 24);
        assert!(text.contains("* picked.txt"), "selection marker missing");
        assert!(text.contains("1 selected"), "status tally not updated");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rename_modal_covers_the_panels_and_shows_the_name() {
        let dir = tempdir("modal-input");
        std::fs::write(dir.join("note.txt"), b"x").unwrap();
        let mut app = app_at(dir.clone());
        app.active_panel_mut().move_down(); // onto note.txt
        app.handle_key_event(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::F(7),
            crossterm::event::KeyModifiers::NONE,
        ));

        let text = render_to_string(&app, 100, 24);
        assert!(text.contains("Rename"), "modal title missing");
        assert!(text.contains("note.txt"), "prefilled name missing");
        assert!(text.contains("apply"), "footer hint missing");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn confirm_modal_states_that_the_action_is_permanent() {
        let dir = tempdir("modal-confirm");
        let mut app = app_at(dir.clone());
        app.modal = crate::app::ModalState::Confirm {
            title: "Overwrite?".into(),
            prompt: "other.txt already exists.".into(),
            action: crate::app::ConfirmAction::RenameOverwrite {
                from: dir.join("a"),
                to: dir.join("other.txt"),
            },
            allow_skip: false,
        };

        let text = render_to_string(&app, 100, 24);
        assert!(text.contains("Overwrite?"));
        assert!(text.contains("cannot be undone"));
        assert!(text.contains("confirm"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn error_report_shows_every_message() {
        let dir = tempdir("modal-error");
        let mut app = app_at(dir.clone());
        app.modal = crate::app::ModalState::ErrorReport {
            title: "Rename failed".into(),
            errors: vec![
                "first: permission denied".into(),
                "second: no space left".into(),
            ],
            scroll_offset: 0,
        };

        let text = render_to_string(&app, 100, 24);
        assert!(text.contains("Rename failed"));
        assert!(text.contains("first: permission denied"));
        assert!(text.contains("second: no space left"));
        assert!(text.contains("dismiss"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn narrow_terminal_does_not_panic() {
        // Degenerate sizes are where layout code divides by zero.
        let dir = tempdir("narrow");
        let app = app_at(dir.clone());
        for (w, h) in [(1, 1), (10, 3), (20, 5), (200, 60)] {
            let _ = render_to_string(&app, w, h);
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
