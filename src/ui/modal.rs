//! Centred overlay dialogs: confirmations, text input, error reports, progress.
//!
//! Every dialog is drawn the same way: compute a centred rectangle, paint
//! `Clear` into it, then draw a bordered block. The `Clear` matters — without it
//! ratatui composites the widget over whatever the panels painted underneath and
//! the dialog text sits on a mess of filenames.
//!
//! Sizing strategy: short dialogs use fixed dimensions clamped to the frame
//! (so a 6-row confirm never becomes 2 rows on a short terminal), while the
//! error report uses a proportion of the screen because its content length is
//! the user's business. `centered_rect` is the percentage helper from the
//! specification; `centered_fixed` is its pixel analogue.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};

use crate::app::{App, ConfirmAction, InputState, ModalState};
use crate::format::{format_size, truncate_path};
use crate::tasks::{ActiveTaskHandle, IoOperation, TaskPhase};
use crate::theme::Theme;

/// A rectangle covering `percent_x`% by `percent_y`% of `area`, centred.
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// A `width` x `height` rectangle centred in `area`, clamped when it would not
/// fit. Clamping keeps a dialog usable (and non-panicking) on tiny terminals.
pub fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Draw whichever modal is open. A no-op when none is.
pub fn render(frame: &mut Frame, app: &App) {
    let theme = app.theme;
    let area = frame.area();

    match &app.modal {
        ModalState::None => {}
        ModalState::Confirm {
            title,
            prompt,
            action,
            allow_skip,
        } => {
            // The headline adapts to what is being confirmed, so the wording
            // never claims "overwrite" for an operation that cannot overwrite.
            let banner = match action {
                ConfirmAction::RenameOverwrite { .. } => "This cannot be undone.",
                ConfirmAction::Task { task } if task.op == IoOperation::Delete => {
                    "This cannot be undone."
                }
                ConfirmAction::Task { .. } => "",
            };

            let hints: &[(&str, &str)] = if *allow_skip {
                &[("Enter", "overwrite"), ("s", "skip"), ("Esc", "cancel")]
            } else {
                &[("Enter", "confirm"), ("Esc", "cancel")]
            };

            render_text_dialog(frame, area, title, prompt, banner, hints, &theme);
        }
        ModalState::Input { title, input, .. } => {
            let rect = centered_fixed(area, 64, 7);
            let block = dialog_block(title, &theme);
            let inner = block.inner(rect);

            frame.render_widget(Clear, rect);
            frame.render_widget(block, rect);

            let field = if inner.width > 2 { inner.width - 2 } else { 1 };
            let field_area = Rect {
                x: inner.x + 1,
                y: inner.y,
                width: field,
                height: 1,
            };
            frame.render_widget(Paragraph::new(input_line(input, &theme)), field_area);

            let footer = Rect {
                y: inner.y + inner.height.saturating_sub(1),
                height: 1,
                ..inner
            };
            frame.render_widget(
                footer_line(&[("Enter", "apply"), ("Esc", "cancel")], &theme),
                footer,
            );
        }
        ModalState::Progress => match &app.active_task {
            Some(task) => render_task_progress(frame, task, area, &theme),
            // Not reachable through normal flow: the modal is only opened by
            // `start_task`, which always installs the handle first.
            None => render_task_progress_placeholder(frame, area, &theme),
        },
        ModalState::ErrorReport {
            title,
            errors,
            scroll_offset,
        } => {
            let rect = centered_rect(80, 60, area);
            let block = dialog_block(title, &theme);
            let inner = block.inner(rect);

            frame.render_widget(Clear, rect);
            frame.render_widget(block, rect);

            let body_height = inner.height.saturating_sub(1);
            let lines: Vec<Line> = errors
                .iter()
                .flat_map(|error| {
                    let mut text = error.as_str();
                    let mut out = Vec::new();
                    // Wrap manually into the available width so long OS errors
                    // remain readable; Paragraph only wraps long strings when
                    // asked, and the count of visual rows then becomes a guess.
                    let width = inner.width.max(20) as usize;
                    while !text.is_empty() {
                        let (chunk, rest) = text.split_at(
                            text.char_indices()
                                .nth(width)
                                .map(|(i, _)| i)
                                .unwrap_or(text.len()),
                        );
                        out.push(Line::from(Span::styled(
                            chunk.to_string(),
                            Style::default().fg(theme.error_fg),
                        )));
                        text = rest;
                    }
                    out
                })
                .collect();

            frame.render_widget(
                Paragraph::new(lines).scroll((*scroll_offset, 0)),
                Rect {
                    height: body_height,
                    ..inner
                },
            );

            let footer = Rect {
                y: inner.y + inner.height.saturating_sub(1),
                height: 1,
                ..inner
            };
            frame.render_widget(
                footer_line(&[("Esc", "dismiss"), ("Up/Down", "scroll")], &theme),
                footer,
            );
        }
    }
}

/// Shared shape for the two dialogs that are just "some text plus a footer".
fn render_text_dialog(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    message: &str,
    banner: &str,
    hints: &[(&str, &str)],
    theme: &Theme,
) {
    let rect = centered_fixed(area, 60, 9);
    let block = dialog_block(title, theme);
    let inner = block.inner(rect);

    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);

    let body = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };

    let lines = vec![
        Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(theme.fg),
        )),
        Line::from(""),
        Line::from(Span::styled(
            banner.to_string(),
            Style::default()
                .fg(theme.error_fg)
                .add_modifier(Modifier::BOLD),
        )),
    ];

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);

    let footer = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    frame.render_widget(footer_line(hints, theme), footer);
}

/// Progress dialog: a gauge, the file being worked on, and the counters.
///
/// All of this is read straight from the handle, which `App::pump_worker`
/// updates from the worker's messages; the renderer never talks to the thread.
fn render_task_progress(frame: &mut Frame, task: &ActiveTaskHandle, area: Rect, theme: &Theme) {
    let rect = centered_fixed(area, 66, 8);
    let title = format!("{}…", task.op.gerund());
    let block = dialog_block(&title, theme);
    let inner = block.inner(rect);

    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let ratio = task.fraction();
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(theme.dir_fg).bg(theme.cursor_bg))
        .ratio(ratio)
        .label(format!("{:.0}%", ratio * 100.0));

    frame.render_widget(gauge, Rect { height: 1, ..inner });

    // Current file, kept to one line by shortening the middle of the path.
    let width = inner.width.saturating_sub(2) as usize;
    let file = if task.current_file.is_empty() {
        "…".to_string()
    } else {
        truncate_path(&task.current_file, width.max(8))
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            file,
            Style::default().fg(theme.fg),
        ))),
        Rect {
            y: inner.y + 2,
            height: 1,
            ..inner
        },
    );

    // Counters: items always, bytes when they are known (delete has none).
    // Before `Started` arrives the totals are genuinely unknown, and "0 / 0
    // items" would read as a finished task rather than a scan in progress.
    let bytes = if task.total_bytes > 0 {
        format!(
            "   {} / {}",
            format_size(task.bytes_done),
            format_size(task.total_bytes)
        )
    } else {
        String::new()
    };
    let counters = if task.phase == TaskPhase::Scanning {
        let counted = if task.total_items > 0 {
            format!(" ({} so far)", task.total_items)
        } else {
            String::new()
        };
        format!("Measuring the work{counted}…")
    } else {
        format!("{} / {} items{}", task.items_done, task.total_items, bytes)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            counters,
            Style::default().fg(theme.border_inactive),
        ))),
        Rect {
            y: inner.y + 3,
            height: 1,
            ..inner
        },
    );

    if task.error_count > 0 && inner.height > 5 {
        let detail = task
            .last_error
            .clone()
            .unwrap_or_else(|| "error".to_string());
        let notice = format!(
            "{} error(s): {}",
            task.error_count,
            truncate_path(&detail, width.max(8))
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                notice,
                Style::default().fg(theme.error_fg),
            ))),
            Rect {
                y: inner.y + 4,
                height: 1,
                ..inner
            },
        );
    }

    // Footer: what the user can do next, or what the app is already doing.
    let footer = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    match task.phase {
        TaskPhase::Cancelling => frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Cancelling… (waiting for the current chunk to finish)",
                Style::default().fg(theme.error_fg),
            ))),
            footer,
        ),
        _ => frame.render_widget(footer_line(&[("Esc", "cancel")], theme), footer),
    }
}

/// Shown for the one frame between opening the progress modal and installing
/// the handle.
fn render_task_progress_placeholder(frame: &mut Frame, area: Rect, theme: &Theme) {
    let rect = centered_fixed(area, 66, 8);
    let block = dialog_block("Working…", theme);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
}

fn dialog_block(title: &str, theme: &Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_active))
        .title(format!(" {title} "))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
}

/// `[key] action  [key] action` in the F-key colours.
fn footer_line(hints: &[(&str, &str)], theme: &Theme) -> Paragraph<'static> {
    let mut spans = Vec::new();
    for (key, label) in hints {
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default().bg(theme.fkey_bg).fg(theme.fkey_fg),
        ));
        spans.push(Span::styled(
            format!("{label}  "),
            Style::default().bg(theme.fkey_bg).fg(theme.fg),
        ));
    }

    Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.fkey_bg))
}

/// The rename field, with a reversed cell standing in for the text cursor.
///
/// A real terminal cursor is not an option: the app hides it globally while in
/// the alternate screen, and unhiding it per-frame would make it flicker.
fn input_line(input: &InputState, theme: &Theme) -> Line<'static> {
    let chars: Vec<char> = input.buffer.chars().collect();
    let pos = input.cursor_pos.min(chars.len());

    let before: String = chars[..pos].iter().collect();
    let under: String = chars
        .get(pos)
        .map(|c| c.to_string())
        .unwrap_or_else(|| " ".to_string());
    let after: String = chars[(pos + 1).min(chars.len())..].iter().collect();

    Line::from(vec![
        Span::styled(before, Style::default().fg(theme.fg)),
        Span::styled(
            under,
            Style::default().bg(theme.cursor_bg).fg(theme.cursor_fg),
        ),
        Span::styled(after, Style::default().fg(theme.fg)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_rect_stays_inside_the_frame_and_is_centred() {
        let area = Rect::new(0, 0, 100, 40);
        let rect = centered_rect(60, 50, area);

        assert!(rect.width <= area.width && rect.height <= area.height);
        assert!(rect.x >= area.x && rect.y >= area.y);
        assert_eq!(rect.x, (area.width - rect.width) / 2);
    }

    #[test]
    fn centered_fixed_clamps_on_a_tiny_frame() {
        let area = Rect::new(0, 0, 10, 3);
        let rect = centered_fixed(area, 60, 9);
        assert_eq!((rect.width, rect.height), (10, 3));
    }

    #[test]
    fn input_line_marks_the_character_under_the_cursor() {
        let theme = crate::theme::ThemeType::CatppuccinMocha.palette();
        let mut input = InputState::new("abc");
        input.home();
        input.right();

        let line = input_line(&input, &theme);
        assert_eq!(line.spans[0].content, "a");
        assert_eq!(line.spans[1].content, "b");
        assert_eq!(line.spans[2].content, "c");
        assert_eq!(line.spans[1].style.bg, Some(theme.cursor_bg));
    }

    /// A handle good enough to render: the progress view only reads fields.
    fn fake_handle(phase: TaskPhase) -> ActiveTaskHandle {
        let (_tx, rx) = std::sync::mpsc::channel();
        let (cancel_tx, _cancel_rx) = std::sync::mpsc::channel();
        ActiveTaskHandle {
            rx,
            cancel_tx,
            op: IoOperation::Copy,
            phase,
            current_file: "/data/big.iso".to_string(),
            items_done: 1,
            total_items: 4,
            bytes_done: 42,
            total_bytes: 100,
            error_count: 0,
            skipped_count: 0,
            last_error: None,
        }
    }

    fn render_progress_text(task: &ActiveTaskHandle) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = crate::theme::ThemeType::TokyoNight.palette();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_task_progress(frame, task, frame.area(), &theme))
            .unwrap();

        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn progress_view_renders_percentage_file_and_counters() {
        let text = render_progress_text(&fake_handle(TaskPhase::Running));

        assert!(text.contains("Copying"));
        assert!(text.contains("42%"), "gauge label missing: {text}");
        assert!(text.contains("big.iso"), "current file missing");
        assert!(text.contains("1 / 4 items"), "counters missing");
        assert!(text.contains("Esc"), "cancel hint missing");
    }

    #[test]
    fn progress_view_says_so_while_cancelling() {
        let text = render_progress_text(&fake_handle(TaskPhase::Cancelling));
        assert!(text.contains("Cancelling"), "no cancelling notice: {text}");
    }

    #[test]
    fn input_line_pads_past_the_end_with_a_cursor_cell() {
        let theme = crate::theme::ThemeType::Dracula.palette();
        let input = InputState::new("ab");
        let line = input_line(&input, &theme);
        assert_eq!(line.spans[0].content, "ab");
        assert_eq!(line.spans[1].content, " ");
        assert_eq!(line.spans[2].content, "");
    }
}
