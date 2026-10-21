//! Rendering of one directory pane.
//!
//! The pane body is a `Table` driven by `PanelState::table_state`. Using a
//! stateful widget rather than a hand-scrolled list is what makes the cursor
//! follow the selection automatically: `Table` adjusts `TableState::offset` so
//! the selected row stays on screen, including after a resize or a sort that
//! moves it. Phase 1 hand-computed that offset; this is both shorter and
//! correct for the cases the hand-rolled version got wrong.
//!
//! Row grammar, fixed by the specification:
//!
//!   `*`  row is selected        `/`  entry is a directory
//!   `@`  entry is a symlink     `!`  symlink target is missing
//!   `>`  row holds the cursor
//!
//! The renderer never stats anything. Every fact it needs (is this a directory,
//! is this link dangling, how big is it) was decided once in `fs::panel`, so a
//! frame costs zero syscalls.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, HighlightSpacing, Paragraph, Row, Table, TableState};

use crate::format::format_size;
use crate::fs::panel::{DirectoryEntry, PanelState};
use crate::theme::Theme;

/// Width of the right-hand size/kind column, matching the spec's ASCII sketch.
const TRAILING_COLUMN: u16 = 14;

/// Draw `panel` into `area`.
///
/// `active` selects the border colour, which is the only cue the user gets that
/// Tab changed the target of navigation.
pub fn render(frame: &mut Frame, panel: &PanelState, area: Rect, active: bool, theme: &Theme) {
    let border = if active {
        theme.border_active
    } else {
        theme.border_inactive
    };
    let title = format!(
        " {} [{} {}] ",
        panel.cwd.display(),
        panel.sort_col.label(),
        if panel.sort_asc {
            "\u{25b2}"
        } else {
            "\u{25bc}"
        }
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(title)
        .style(Style::default().bg(theme.bg).fg(theme.fg));

    let inner = block.inner(area);

    let rows: Vec<Row> = panel
        .entries
        .iter()
        .map(|entry| {
            let is_selected = panel.selected.contains(&entry.path);
            Row::new(vec![
                name_cell(entry, is_selected, theme),
                trailing_cell(entry, theme),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [Constraint::Min(10), Constraint::Length(TRAILING_COLUMN)],
    )
    .block(block)
    // Reserving the symbol width on every row (not just the highlighted one)
    // keeps the names aligned in a column as the cursor moves.
    .highlight_spacing(HighlightSpacing::Always)
    .highlight_symbol("> ")
    .row_highlight_style(Style::default().bg(theme.cursor_bg).fg(theme.cursor_fg));

    // `TableState` is two words (offset + selected index), so cloning it costs
    // nothing next to the frame itself — and it avoids threading `&mut App`
    // through the entire render tree just to satisfy `render_stateful_widget`.
    let mut state: TableState = panel.table_state.clone();

    frame.render_stateful_widget(table, area, &mut state);

    if panel.is_empty() {
        render_empty_hint(frame, inner, theme);
    }
}

/// The name column: marker prefix, then the entry name.
fn name_cell(entry: &DirectoryEntry, is_selected: bool, theme: &Theme) -> Line<'static> {
    let marker = if is_selected {
        '*'
    } else if entry.is_broken_symlink {
        '!'
    } else if entry.is_symlink {
        '@'
    } else if entry.is_dir {
        '/'
    } else {
        ' '
    };

    let name_style = if entry.is_broken_symlink {
        Style::default().fg(theme.error_fg)
    } else if entry.is_dir {
        Style::default().fg(theme.dir_fg)
    } else if entry.is_symlink {
        Style::default().fg(theme.symlink_fg)
    } else {
        Style::default().fg(theme.fg)
    };

    let name_style = if is_selected {
        name_style
            .fg(theme.selected_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        name_style
    };

    Line::from(vec![
        Span::styled(format!("{marker} "), Style::default().fg(theme.fg)),
        Span::styled(entry.name.clone(), name_style),
    ])
}

/// The right-hand column: a kind label for directories and links, a size for
/// regular files. Right-aligned so numbers line up.
fn trailing_cell(entry: &DirectoryEntry, theme: &Theme) -> Line<'static> {
    let text = match kind_label(entry) {
        Some(kind) => kind.to_string(),
        None => format_size(entry.size),
    };

    Line::from(Span::styled(
        text,
        Style::default().fg(theme.border_inactive),
    ))
    .alignment(Alignment::Right)
}

/// Shown over an empty pane so a blank rectangle is unambiguous.
fn render_empty_hint(frame: &mut Frame, area: Rect, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let hint = Paragraph::new(Line::from(Span::styled(
        "empty directory",
        Style::default().fg(theme.border_inactive),
    )))
    .alignment(Alignment::Center);

    let y = area.y + area.height / 2;
    frame.render_widget(hint, Rect::new(area.x, y, area.width, 1));
}

/// The type column shown after the name.
fn kind_label(entry: &DirectoryEntry) -> Option<&'static str> {
    if entry.is_parent_nav {
        Some("<DIR>")
    } else if entry.is_broken_symlink {
        Some("<BROKEN>")
    } else if entry.is_symlink && entry.is_dir {
        Some("<SYM/DIR>")
    } else if entry.is_symlink {
        Some("<SYM>")
    } else if entry.is_dir {
        Some("<DIR>")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_labels_distinguish_symlink_kinds() {
        let base = |is_dir, is_symlink, broken| DirectoryEntry {
            name: "e".into(),
            path: "/e".into(),
            is_dir,
            is_symlink,
            is_broken_symlink: broken,
            is_parent_nav: false,
            size: 0,
            modified: None,
        };

        assert_eq!(kind_label(&base(true, false, false)), Some("<DIR>"));
        assert_eq!(kind_label(&base(false, true, false)), Some("<SYM>"));
        assert_eq!(kind_label(&base(true, true, false)), Some("<SYM/DIR>"));
        assert_eq!(kind_label(&base(false, true, true)), Some("<BROKEN>"));
        assert_eq!(kind_label(&base(false, false, false)), None);
    }
}
